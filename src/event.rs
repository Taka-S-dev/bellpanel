//! The common event format (DESIGN.md §3) and the mapping from each agent's
//! hook payload into it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const FORMAT_VERSION: u32 = 1;
const TEXT_LIMIT: usize = 200;
/// A finished turn's reply is kept longer: it is what the event log shows of
/// each session, and it has the room to show it. Still well under 4 KiB as
/// a line, so one write appends it whole (DESIGN.md §2).
const REPLY_LIMIT: usize = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Copilot,
    Codex,
}

impl Agent {
    pub const ALL: [Agent; 3] = [Agent::Claude, Agent::Copilot, Agent::Codex];

    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Copilot => "copilot",
            Agent::Codex => "codex",
        }
    }

    /// The executable the agent runs as.
    pub fn exe(self) -> &'static str {
        self.name()
    }

    pub fn parse(s: &str) -> Option<Agent> {
        Agent::ALL.into_iter().find(|a| a.name() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Start,
    Prompt,
    Waiting,
    Resumed,
    Stop,
    End,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub v: u32,
    pub at: u64,
    pub agent: Agent,
    pub kind: Kind,
    pub event: String,
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_exe: Option<String>,
    /// The agent's own process, to find its terminal tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_pid: Option<u32>,
    /// `WEZTERM_PANE` of the hook, when the session runs in WezTerm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A prompt the agent or its tooling submitted, not the person: Claude
    /// Code's `<task-notification>` when a background task reports back.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub automatic: bool,
}

fn str_field<'a>(payload: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| payload.get(*k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
}

/// Cuts to `limit` characters on one line.
pub fn clip(text: &str, limit: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        return flat;
    }
    let mut cut: String = flat.chars().take(limit - 1).collect();
    cut.push('…');
    cut
}

/// Blocks an editor or the agent adds around what the person typed: the file
/// open in the IDE, the selection, reminders. They are dropped with their
/// content, or the person's words end up past the cut-off.
const CONTEXT_BLOCKS: [&str; 4] = [
    "ide_opened_file",
    "ide_selection",
    "ide_diagnostics",
    "system-reminder",
];

fn drop_context_blocks(text: &str) -> String {
    let mut out = text.to_string();
    for tag in CONTEXT_BLOCKS {
        let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
        while let Some(start) = out.find(&open) {
            let end = out[start..]
                .find(&close)
                .map_or(out.len(), |e| start + e + close.len());
            out.replace_range(start..end, " ");
        }
    }
    out
}

/// Text without the Markdown marks that read as noise once a reply is run
/// into one line: emphasis and code marks, heading marks, list bullets, and
/// table pipes and rules. A table's cells are kept, separated by `·`.
pub fn plain(text: &str) -> String {
    let text = text.replace("**", "").replace("__", "").replace('`', "");
    let mut out: Vec<&str> = Vec::new();
    for word in text.split_whitespace() {
        let markup = word.chars().all(|c| matches!(c, '|' | '-' | ':'));
        let heading = word.len() <= 6 && word.chars().all(|c| c == '#');
        if markup && (word.contains('|') || word.len() >= 3) {
            // A pipe or a rule between cells; one separator for a run of them.
            if out.last().is_some_and(|last| *last != "·") {
                out.push("·");
            }
        } else if !(heading || word == "-") {
            out.push(word);
        }
    }
    if out.last() == Some(&"·") {
        out.pop();
    }
    out.join(" ")
}

/// Whether a prompt was submitted by the agent's tooling rather than typed:
/// those are markup from their first character, once the context blocks an
/// editor adds are dropped.
pub fn is_automatic_prompt(prompt: &str) -> bool {
    drop_context_blocks(prompt).trim_start().starts_with('<')
}

/// What a person would call the prompt. Context blocks the editor adds are
/// dropped. Claude Code also submits prompts of its own, such as
/// `<task-notification> <summary>…</summary> …` when a background task
/// reports back; those show their summary, and any other markup is dropped.
pub fn readable_prompt(prompt: &str) -> String {
    let without_context = drop_context_blocks(prompt);
    let trimmed = without_context.trim_start();
    if !trimmed.starts_with('<') {
        return plain(trimmed.trim_end());
    }
    if let Some(start) = trimmed.find("<summary>") {
        let body = &trimmed[start + "<summary>".len()..];
        let end = body.find("</summary>").unwrap_or(body.len());
        return body[..end].trim().to_string();
    }
    let mut out = String::new();
    let mut in_tag = false;
    for c in trimmed.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Which kind of moment an agent's hook event stands for, or `None` for one
/// the dashboard has no use for.
pub fn classify(agent: Agent, event: &str, payload: &Value) -> Option<Kind> {
    let notification_type = str_field(payload, &["notification_type", "notificationType"]);
    match (agent, event) {
        // Claude Code also sends SessionStart when it compacts the
        // conversation, which can happen in the middle of a turn: the session
        // goes on as it was.
        (_, "SessionStart" | "sessionStart")
            if str_field(payload, &["source"]) == Some("compact") =>
        {
            None
        }
        (_, "SessionStart" | "sessionStart") => Some(Kind::Start),
        (_, "UserPromptSubmit" | "userPromptSubmitted") => Some(Kind::Prompt),
        (_, "PostToolUse" | "postToolUse") => Some(Kind::Resumed),
        (_, "Stop" | "agentStop") => Some(Kind::Stop),
        (_, "SessionEnd" | "sessionEnd") => Some(Kind::End),
        // Copilot's permissionRequest fires before its rules engine, so also for
        // calls it will allow on its own: not a sign that anyone is waiting.
        (Agent::Copilot, "permissionRequest") => None,
        (_, "PermissionRequest") => Some(Kind::Waiting),
        (_, "Notification" | "notification") => match notification_type {
            Some("permission_prompt" | "elicitation_dialog") => Some(Kind::Waiting),
            Some(_) => None,
            None => {
                let message = str_field(payload, &["message"]).unwrap_or_default();
                message
                    .to_lowercase()
                    .contains("permission")
                    .then_some(Kind::Waiting)
            }
        },
        _ => None,
    }
}

/// Turns one hook call into an event, or `None` when there is nothing to record.
///
/// `agent` comes from the hook's command line. A camelCase payload overrides
/// it: only Copilot sends `sessionId`, and Copilot also runs Claude's
/// project-level hooks in trusted folders.
pub fn normalize(agent: Agent, event: &str, payload: &Value, now_ms: u64) -> Option<Event> {
    let agent = if payload.get("sessionId").is_some() {
        Agent::Copilot
    } else {
        agent
    };
    let kind = classify(agent, event, payload)?;
    let session = str_field(payload, &["session_id", "sessionId"])?.to_string();

    let text = match kind {
        Kind::Prompt => str_field(payload, &["prompt"]).map(readable_prompt),
        Kind::Start => {
            str_field(payload, &["initialPrompt", "initial_prompt"]).map(readable_prompt)
        }
        Kind::Stop => {
            str_field(payload, &["last_assistant_message", "lastAssistantMessage"]).map(plain)
        }
        Kind::Waiting => {
            str_field(payload, &["message", "tool_name", "toolName"]).map(str::to_string)
        }
        // Why it ended (`clear`, `prompt_input_exit`, `complete`, ...), to tell
        // a closed tab from a cleared conversation afterwards.
        // Codex always says "other", which tells nothing.
        Kind::End => str_field(payload, &["reason"])
            .filter(|r| *r != "other")
            .map(str::to_string),
        Kind::Resumed => None,
    }
    .map(|t| {
        clip(
            &t,
            if kind == Kind::Stop {
                REPLY_LIMIT
            } else {
                TEXT_LIMIT
            },
        )
    })
    .filter(|t| !t.is_empty());

    Some(Event {
        v: FORMAT_VERSION,
        at: now_ms,
        agent,
        kind,
        event: event.to_string(),
        session,
        turn: str_field(payload, &["turn_id", "turnId"]).map(str::to_string),
        cwd: str_field(payload, &["cwd"]).unwrap_or_default().to_string(),
        host_pid: None,
        host_exe: None,
        agent_pid: None,
        pane: None,
        text,
        automatic: kind == Kind::Prompt
            && str_field(payload, &["prompt"]).is_some_and(is_automatic_prompt),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Payloads below are trimmed from real ones captured in spike/events.jsonl.

    #[test]
    fn claude_stop_is_a_stop() {
        let p = json!({"session_id":"6c96","transcript_path":"x","cwd":"C:\\w\\sandbox","hook_event_name":"Stop"});
        let e = normalize(Agent::Claude, "Stop", &p, 5).unwrap();
        assert_eq!(
            (e.agent, e.kind, e.session.as_str(), e.cwd.as_str()),
            (Agent::Claude, Kind::Stop, "6c96", "C:\\w\\sandbox")
        );
        assert_eq!(e.text, None);
    }

    #[test]
    fn copilot_payload_is_camel_case() {
        let p = json!({"sessionId":"7aae","timestamp":1794733015787u64,"cwd":"C:\\w","prompt":"Reply with just: hi"});
        let e = normalize(Agent::Copilot, "userPromptSubmitted", &p, 1).unwrap();
        assert_eq!((e.kind, e.session.as_str()), (Kind::Prompt, "7aae"));
        assert_eq!(e.text.as_deref(), Some("Reply with just: hi"));
    }

    #[test]
    fn codex_stop_keeps_the_reply_and_turn() {
        let p = json!({"session_id":"01a1","turn_id":"t1","cwd":"C:\\w","hook_event_name":"Stop","model":"m","permission_mode":"p","stop_hook_active":false,"last_assistant_message":"hi"});
        let e = normalize(Agent::Codex, "Stop", &p, 1).unwrap();
        assert_eq!(
            (e.kind, e.turn.as_deref(), e.text.as_deref()),
            (Kind::Stop, Some("t1"), Some("hi"))
        );
    }

    #[test]
    fn a_camel_case_payload_is_copilot_whatever_the_command_line_says() {
        let p = json!({"sessionId":"s","cwd":"c"});
        assert_eq!(
            normalize(Agent::Claude, "Stop", &p, 1).unwrap().agent,
            Agent::Copilot
        );
    }

    #[test]
    fn copilot_permission_request_is_not_waiting() {
        let p = json!({"sessionId":"s","toolName":"shell"});
        assert_eq!(normalize(Agent::Copilot, "permissionRequest", &p, 1), None);
    }

    #[test]
    fn notifications_count_only_when_they_ask_for_something() {
        let ask = json!({"session_id":"s","notification_type":"permission_prompt","message":"Claude needs your permission to use Bash"});
        let idle = json!({"session_id":"s","notification_type":"idle_prompt","message":"Claude is waiting for your input"});
        let copilot =
            json!({"sessionId":"s","notification_type":"elicitation_dialog","message":"?"});
        assert_eq!(
            normalize(Agent::Claude, "Notification", &ask, 1)
                .unwrap()
                .kind,
            Kind::Waiting
        );
        assert_eq!(normalize(Agent::Claude, "Notification", &idle, 1), None);
        assert_eq!(
            normalize(Agent::Copilot, "notification", &copilot, 1)
                .unwrap()
                .kind,
            Kind::Waiting
        );
    }

    #[test]
    fn untyped_notification_falls_back_to_the_message() {
        let p = json!({"session_id":"s","message":"Claude needs your permission to use Bash"});
        assert_eq!(
            normalize(Agent::Claude, "Notification", &p, 1)
                .unwrap()
                .kind,
            Kind::Waiting
        );
    }

    #[test]
    fn no_session_id_means_nothing_to_record() {
        assert_eq!(
            normalize(Agent::Claude, "Stop", &json!({"cwd":"c"}), 1),
            None
        );
    }

    #[test]
    fn compacting_is_not_a_session_start() {
        let start = |source| {
            normalize(
                Agent::Claude,
                "SessionStart",
                &json!({"session_id":"s","source":source}),
                1,
            )
            .map(|e| e.kind)
        };
        assert_eq!(start("compact"), None);
        assert_eq!(start("resume"), Some(Kind::Start));
    }

    #[test]
    fn unknown_events_are_ignored() {
        assert_eq!(
            normalize(Agent::Codex, "PreCompact", &json!({"session_id":"s"}), 1),
            None
        );
    }

    #[test]
    fn injected_prompts_show_their_summary() {
        // Trimmed from a real UserPromptSubmit payload.
        let injected = "<task-notification> <summary>Commit security review found: Symlink in settings.py</summary> </task-notification> <system-reminder> Background…";
        assert_eq!(
            readable_prompt(injected),
            "Commit security review found: Symlink in settings.py"
        );
        assert_eq!(readable_prompt("<foo>bar</foo>"), "bar");
        assert_eq!(
            readable_prompt("use <T> here"),
            "use <T> here",
            "a person's prompt is left alone"
        );
        // Such prompts are marked as not typed by the person.
        let p = json!({"session_id": "s", "prompt": injected});
        assert!(
            normalize(Agent::Claude, "UserPromptSubmit", &p, 1)
                .unwrap()
                .automatic
        );
        let p = json!({"session_id": "s", "prompt": "use <T> here"});
        assert!(
            !normalize(Agent::Claude, "UserPromptSubmit", &p, 1)
                .unwrap()
                .automatic
        );
    }

    #[test]
    fn an_end_keeps_its_reason() {
        let claude = json!({"session_id":"s","reason":"prompt_input_exit"});
        assert_eq!(
            normalize(Agent::Claude, "SessionEnd", &claude, 1)
                .unwrap()
                .text
                .as_deref(),
            Some("prompt_input_exit")
        );
        let copilot = json!({"sessionId":"s","reason":"complete"});
        assert_eq!(
            normalize(Agent::Copilot, "sessionEnd", &copilot, 1)
                .unwrap()
                .text
                .as_deref(),
            Some("complete")
        );
    }

    #[test]
    fn editor_context_is_dropped_from_prompts() {
        // Shaped like a real prompt from the VS Code extension.
        let prompt = "<ide_opened_file>The user opened the file c:\\x\\README.md in the IDE. This may or may not be related.</ide_opened_file>切り替わりました。";
        assert_eq!(readable_prompt(prompt), "切り替わりました。");
        let reminder = "<system-reminder>be brief</system-reminder>\nfix the build";
        assert_eq!(readable_prompt(reminder), "fix the build");
    }

    #[test]
    fn headings_bullets_and_tables_read_as_text() {
        assert_eq!(
            plain("## 結果 | # | 項目 | 結果 | |---|---|---| | 1 | ビルド | ✅ |"),
            "結果 · 項目 · 結果 · 1 · ビルド · ✅"
        );
        assert_eq!(plain("- one\n- two\n---\nthree"), "one two · three");
        // Hyphens inside words, negative numbers and a lone colon stay.
        assert_eq!(plain("re-run -5 : ok"), "re-run -5 : ok");
    }

    #[test]
    fn markdown_marks_are_dropped() {
        assert_eq!(
            plain("**効果が大きい** は `i` キー"),
            "効果が大きい は i キー"
        );
        let p = json!({"session_id":"s","last_assistant_message":"**Done.** Ran `cargo test`."});
        assert_eq!(
            normalize(Agent::Claude, "Stop", &p, 1)
                .unwrap()
                .text
                .as_deref(),
            Some("Done. Ran cargo test.")
        );
    }

    #[test]
    fn long_text_is_cut_to_one_line() {
        let long = format!("line one\n{}", "x".repeat(500));
        let clipped = clip(&long, TEXT_LIMIT);
        assert!(!clipped.contains('\n'));
        assert_eq!(clipped.chars().count(), TEXT_LIMIT);
        assert!(clipped.ends_with('…'));
        // A reply is kept longer than a prompt.
        let longer = "y".repeat(1_000);
        let reply = json!({"session_id": "s", "last_assistant_message": longer});
        let stop = normalize(Agent::Claude, "Stop", &reply, 1).unwrap();
        assert_eq!(stop.text.unwrap().chars().count(), REPLY_LIMIT);
        let prompt = json!({"session_id": "s", "prompt": longer});
        let asked = normalize(Agent::Claude, "UserPromptSubmit", &prompt, 1).unwrap();
        assert_eq!(asked.text.unwrap().chars().count(), TEXT_LIMIT);
    }

    #[test]
    fn events_round_trip_through_json() {
        let p = json!({"session_id":"s","cwd":"c","prompt":"p"});
        let e = normalize(Agent::Claude, "UserPromptSubmit", &p, 42).unwrap();
        let line = serde_json::to_string(&e).unwrap();
        assert!(
            !line.contains("host_pid"),
            "absent fields stay out of the line: {line}"
        );
        assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), e);
    }
}
