//! bellpanel: the dashboard, plus `install` / `uninstall`. See docs/DESIGN.md.

use std::io::{self, Write};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bellpanel::event::{plain, Agent, Kind};
use bellpanel::state::{Board, Key, Mark, Order, Session, Slot, State};
use bellpanel::store::{self, Tail};
use bellpanel::watch::{CommandWatch, Waiter};
use bellpanel::{host, install};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyCode, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use unicode_width::UnicodeWidthChar;

/// How often the loop looks for new events and redraws when nothing moves.
const TICK: Duration = Duration::from_millis(250);
/// One spinner frame, the pace braille spinners are usually run at (80 to
/// 100 ms); the loop redraws this often only while a session works.
const SPINNER_FRAME: Duration = Duration::from_millis(100);
/// How often running processes are listed while a session waits.
const WATCH_EVERY: Duration = Duration::from_secs(1);
/// Braille dots: one column wide everywhere. The circle quarters used before
/// mix East Asian ambiguous widths (◐ ◑) with narrow ones (◓ ◒), so a
/// terminal drawing ambiguous characters wide shifted the row every frame.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// The timeline column: one slot per minute, up to the last half hour.
const TIMELINE_SLOTS: usize = 30;
const SLOT_MS: u64 = 60_000;
/// Columns a row needs before the timeline: state, agent, app, folder, age, borders.
const ROW_FIXED_WIDTH: usize = 50;
/// Columns kept for the prompt or reply, which matter more than the timeline.
const ROW_TEXT_MIN_WIDTH: usize = 16;
/// Fewer minutes than this is not worth showing.
const TIMELINE_MIN_SLOTS: usize = 10;

/// How many minutes of timeline fit, or 0 to leave it out.
fn timeline_slots(width: u16, icons: bool) -> usize {
    let fixed = ROW_FIXED_WIDTH + if icons { ICON_WIDTH } else { 0 };
    let room = (width as usize).saturating_sub(fixed + 2 + ROW_TEXT_MIN_WIDTH);
    let slots = room.min(TIMELINE_SLOTS);
    if slots >= TIMELINE_MIN_SLOTS {
        slots
    } else {
        0
    }
}
/// The footer when nothing else is shown: short enough for a narrow pane.
const HELP: &str = "click a folder to open · ? keys · q quit";
/// The full list, shown by `?`.
const KEYS: [(&str, &str); 10] = [
    (
        "click folder, Enter",
        "bring that session's window and tab forward",
    ),
    ("click row, j / k", "select"),
    ("Space", "mark a finished session as seen"),
    ("d", "hide the row until the session does something new"),
    ("e", "event log: selected session, every session, hidden"),
    ("s", "order: as they appeared, or by urgency"),
    ("i", "Nerd Font icons on or off"),
    ("wheel", "move the selection"),
    ("?", "this list"),
    ("q", "quit"),
];
/// How long an ordinary status message stays; errors stay three times as long.
const STATUS_SECS: u64 = 5;
/// Where the folder name starts in a row: after the state (7), agent (8) and
/// app (5) columns; the agent column is two wider with icons.
fn folder_column(icons: bool) -> u16 {
    21 + if icons { ICON_WIDTH as u16 } else { 0 }
}
const APP_WIDTH: usize = 4;
const AGENT_WIDTH: usize = 7;
/// An icon and the space after it.
const ICON_WIDTH: usize = 2;
const FOLDER_WIDTH: usize = 20;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let result = match args.first().map(String::as_str) {
        None => run_dashboard(),
        Some("install") => match chosen_agents(&args) {
            Ok(agents) => run_install(true, dry_run, agents),
            Err(e) => Err(io::Error::new(io::ErrorKind::InvalidInput, e)),
        },
        Some("uninstall") => run_install(false, dry_run, None),
        // Internal: run by the dashboard in a process of its own, since reading
        // another console's title means detaching from this one.
        Some("console-title") => {
            if let Some(title) = args
                .get(1)
                .and_then(|pid| pid.parse().ok())
                .and_then(bellpanel::tabs::attach_and_read_title)
            {
                print!("{title}");
            }
            Ok(())
        }
        // Troubleshooting: open the window (and tab) of <agent> working in <dir>.
        Some("open") => {
            match (args.get(1), args.get(2)) {
                (Some(agent), Some(dir)) => {
                    let target = host::Target {
                        agent_exe: agent.clone(),
                        host: None,
                        pane: None,
                        cwd: dir.clone(),
                    };
                    match host::focus_session(&target) {
                        Ok(tab) => println!(
                            "opened{}",
                            tab.map(|t| format!(" · {t}")).unwrap_or_default()
                        ),
                        Err(e) => println!("{e}"),
                    }
                }
                _ => eprintln!("usage: bellpanel open <agent-exe> <dir>"),
            }
            Ok(())
        }
        // Troubleshooting: which window would a session of <agent> in <dir> open?
        Some("which-host") => {
            match (args.get(1), args.get(2)) {
                (Some(agent), Some(dir)) => match host::agent_host(agent, dir) {
                    Some(found) => {
                        let title = found.agent_pid.and_then(bellpanel::tabs::console_title);
                        println!(
                            "{} (pid {}), agent pid {:?}, tab title {title:?}",
                            found.exe, found.pid, found.agent_pid
                        )
                    }
                    None => println!("no {agent} process working in {dir} runs under a window"),
                },
                _ => eprintln!("usage: bellpanel which-host <agent-exe> <dir>"),
            }
            Ok(())
        }
        Some("-h" | "--help" | "help") => {
            print_usage();
            Ok(())
        }
        Some("-V" | "--version" | "version") => {
            println!("bellpanel {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n");
            print_usage();
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("bellpanel: {e}");
        std::process::exit(1);
    }
}

fn print_usage() {
    println!(
        "bellpanel             show the dashboard\n\
         bellpanel install     add hooks to the agents found here (Claude Code, Copilot CLI, Codex)\n\
         bellpanel uninstall   remove them\n\
         \n  --dry-run           print what would change, write nothing\n\
         \x20 --agents a,b        install for these agents only (claude, copilot, codex)\n\
         \nWhen a click opens the wrong window, or none:\n\
         bellpanel which-host <agent> <dir>   say which window a session there would open\n\
         bellpanel open <agent> <dir>         open it\n\
         \nbellpanel --version   print the version\n\
         \nEvents: {}",
        store::events_path().display()
    );
}

/// `--agents claude,codex` from the command line, if given.
fn chosen_agents(args: &[String]) -> Result<Option<Vec<Agent>>, String> {
    let value = args
        .iter()
        .position(|a| a == "--agents")
        .map(|i| args.get(i + 1).cloned().unwrap_or_default())
        .or_else(|| {
            args.iter()
                .find_map(|a| a.strip_prefix("--agents=").map(str::to_string))
        });
    let Some(value) = value else { return Ok(None) };
    value
        .split(',')
        .map(|name| {
            Agent::parse(name.trim())
                .ok_or_else(|| format!("unknown agent `{name}`; use claude, copilot, codex"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn run_install(install: bool, dry_run: bool, chosen: Option<Vec<Agent>>) -> io::Result<()> {
    let exe = install::installed_hook_path();
    let agents = chosen.clone().unwrap_or_else(install::present_agents);
    if install {
        for agent in Agent::ALL.into_iter().filter(|a| !agents.contains(a)) {
            if chosen.is_some() {
                println!("skip {}: not chosen", agent.name());
            } else {
                println!(
                    "skip {}: no ~/.{} folder, so it does not look installed",
                    agent.name(),
                    agent.name()
                );
            }
        }
    }
    let changes = install::plan(install.then_some(exe.as_path()), &agents)?;
    if changes.is_empty() {
        println!("Nothing to change.");
    }
    for change in &changes {
        let verb = match (&change.before, &change.after) {
            (None, _) => "create",
            (_, None) => "delete",
            _ => "update",
        };
        println!("{verb} {}", change.path.display());
        if dry_run {
            if let Some(after) = &change.after {
                println!("{after}");
            }
        }
    }
    if dry_run {
        if install {
            println!("would copy bellpanel-hook to {}", exe.display());
        }
        return Ok(());
    }
    if install {
        let copied = install::copy_hook_binary()?;
        println!("copied hook to {}", copied.display());
        for file in install::migrate_legacy_data()? {
            println!("carried over {}", file.display());
        }
    }
    install::apply(&changes)?;
    if install {
        println!("\nDone. Restart running agents so they read their hooks.");
        if agents.contains(&Agent::Codex) {
            println!(
                "Codex runs new hooks only after you trust them: open `codex` and use /hooks."
            );
        }
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn age(now: u64, at: u64) -> String {
    let secs = now.saturating_sub(at) / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn agent_color(agent: Agent) -> Color {
    match agent {
        Agent::Claude => Color::Rgb(217, 119, 87),
        Agent::Copilot => Color::Rgb(120, 140, 255),
        Agent::Codex => Color::Rgb(16, 163, 127),
    }
}

const LINK_COLOR: Color = Color::Rgb(110, 170, 255);
const SELECTED_BG: Color = Color::Rgb(58, 64, 78);
const WAIT_COLOR: Color = Color::Rgb(240, 180, 40);
const DONE_COLOR: Color = Color::Rgb(90, 200, 120);
const ERROR_COLOR: Color = Color::Rgb(230, 90, 90);

/// What the event log under the sessions shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum EventsMode {
    /// The selected session's prompts and replies.
    #[default]
    Selected,
    /// Each listed session's latest event, wrapped to the room there is:
    /// what every terminal is at, in one look.
    Latest,
    /// Every session's events in one stream.
    All,
    /// No log: the sessions get the whole pane.
    Off,
}

impl EventsMode {
    fn next(self) -> EventsMode {
        match self {
            EventsMode::Selected => EventsMode::Latest,
            EventsMode::Latest => EventsMode::All,
            EventsMode::All => EventsMode::Off,
            EventsMode::Off => EventsMode::Selected,
        }
    }

    fn label(self) -> &'static str {
        match self {
            EventsMode::Selected => "events: the selected session",
            EventsMode::Latest => "events: each session's latest",
            EventsMode::All => "events: every session",
            EventsMode::Off => "events: hidden",
        }
    }
}

/// What the terminal bell rings for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Bell {
    /// A session starting to wait, and one finishing.
    #[default]
    Both,
    /// Only a session starting to wait: the one that is stuck without you.
    Waits,
    Off,
}

impl Bell {
    fn next(self) -> Bell {
        match self {
            Bell::Both => Bell::Waits,
            Bell::Waits => Bell::Off,
            Bell::Off => Bell::Both,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Bell::Both => "bell: waits and finishes",
            Bell::Waits => "bell: waits only",
            Bell::Off => "bell: off",
        }
    }

    fn rings(self, kind: Kind) -> bool {
        match self {
            Bell::Both => true,
            Bell::Waits => kind == Kind::Waiting,
            Bell::Off => false,
        }
    }
}

/// Preferences kept in `settings.json`. Unknown or missing fields fall back
/// to their defaults, so an old or hand-edited file still loads.
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Settings {
    sort: Order,
    events: EventsMode,
    /// Nerd Font icons next to agent names. Off by default: the dashboard
    /// cannot tell which font the terminal draws with, and without the
    /// glyphs every icon is an empty box.
    icons: bool,
    bell: Bell,
}

impl Settings {
    fn load() -> Settings {
        std::fs::read_to_string(store::settings_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> io::Result<()> {
        let path = store::settings_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(
            path,
            serde_json::to_string_pretty(self).map_err(io::Error::other)?,
        )
    }
}

struct App {
    settings: Settings,
    /// Agents whose config calls our hook, read once at start-up.
    agents: Vec<Agent>,
    board: Board,
    tail: Tail,
    selected: Option<Key>,
    /// Keys of the rows on screen, top to bottom, from the last draw.
    visible: Vec<Key>,
    /// Display width of each row's folder link, matching `visible`.
    visible_folder_widths: Vec<usize>,
    /// Screen area of the list's rows, from the last draw.
    rows_area: Rect,
    list_offset: usize,
    status: Option<(String, bool)>,
    /// The status as last seen, and since when, to let it fade.
    status_seen: Option<((String, bool), std::time::Instant)>,
    /// The key list from `?` is open.
    show_keys: bool,
    /// The row whose folder link is under the pointer.
    hover: Option<usize>,
    /// A window being brought forward on another thread; its status line.
    opening: Option<mpsc::Receiver<(String, bool)>>,
    /// Another row was asked for while that was going on: open the selected
    /// one when it is done.
    open_next: bool,
    /// Where the selection was in the list at the last draw, so that it
    /// stays in place when its row goes.
    selected_at: usize,
    /// The bell has rung and the request has not been withdrawn yet.
    attention_asked: bool,
    /// New processes while a session waits, to see an approved command start.
    commands: CommandWatch,
    /// When processes were last listed for `commands`.
    watched_at: std::time::Instant,
    /// When the dashboard started; the spinner's frame follows the clock, so
    /// keys pressed do not speed it up.
    started: std::time::Instant,
}

fn run_dashboard() -> io::Result<()> {
    let mut app = App {
        settings: Settings::load(),
        agents: install::installed_agents(),
        board: Board::default(),
        tail: Tail::open(store::events_path()),
        selected: None,
        visible: Vec::new(),
        visible_folder_widths: Vec::new(),
        rows_area: Rect::default(),
        list_offset: 0,
        status: None,
        hover: None,
        opening: None,
        open_next: false,
        selected_at: 0,
        status_seen: None,
        show_keys: false,
        attention_asked: false,
        commands: CommandWatch::default(),
        watched_at: std::time::Instant::now(),
        started: std::time::Instant::now(),
    };
    // Replay what is already there without ringing for it, then what the
    // user had already dealt with.
    for event in app.tail.read_new()? {
        app.board.apply(&event);
    }
    for mark in store::read_all::<Mark>(&store::marks_path()) {
        app.board.apply_mark(&mark);
    }

    let mut terminal = ratatui::init();
    execute!(io::stdout(), EnableMouseCapture)?;
    let result = event_loop(&mut terminal, &mut app);
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Writes bytes straight to the terminal, past ratatui's buffer.
fn write_raw(bytes: &[u8]) {
    let mut out = io::stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

/// Ends the wait of sessions whose approved command has started: no hook
/// reports that until the command finishes (watch.rs).
fn watch_commands(app: &mut App) {
    let waiting: Vec<&Session> = app.board.waiting().collect();
    if waiting.is_empty() {
        app.commands.stop();
        return;
    }
    let waiters: Vec<Waiter> = waiting
        .iter()
        .map(|s| Waiter {
            key: s.key(),
            agent_exe: s.agent.exe(),
            agent_pid: s.agent_pid,
            cwd: &s.cwd,
            since: s.since,
            alone: app.board.busy(s.agent) == 1,
        })
        .collect();
    let now = now_ms();
    let started = app
        .commands
        .poll(now, &host::processes(), &waiters, host::process_cwd);
    for key in started {
        app.board.command_started(&key, now);
    }
}

/// Whether any listed session still waits for the user. Unseen finishes do
/// not hold the mark: a wait answered in the agent's window should clear it.
fn waits_for_user(app: &App) -> bool {
    app.board
        .rows(now_ms())
        .iter()
        .any(|s| s.state == State::Waiting)
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> io::Result<()> {
    loop {
        if let Some(rx) = &app.opening {
            match rx.try_recv() {
                Ok(status) => {
                    app.status = Some(status);
                    app.opening = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => app.opening = None,
            }
            if app.opening.is_none() && std::mem::take(&mut app.open_next) {
                open_selected(app);
            }
        }
        let mut ring = false;
        match app.tail.read_new() {
            Ok(events) => {
                for event in events {
                    ring |= app.board.apply(&event) && app.settings.bell.rings(event.kind);
                }
            }
            Err(e) => {
                app.status = Some((
                    format!("cannot read {}: {e}", app.tail.path().display()),
                    true,
                ))
            }
        }
        // Listing processes is too costly for every redraw.
        if app.watched_at.elapsed() >= WATCH_EVERY {
            app.watched_at = std::time::Instant::now();
            watch_commands(app);
        }
        fade_status(app);
        terminal.draw(|frame| draw(frame, app))?;
        if ring {
            // terminal-pane turns BEL into a tab mark and a taskbar flash.
            write_raw(b"\x07");
            // A bell for a finish alone is never withdrawn: it stays until
            // the user looks.
            app.attention_asked = waits_for_user(app);
        } else if app.attention_asked && !waits_for_user(app) {
            // Answered in the agent's own window, or dismissed here: withdraw
            // the request so the tab mark goes too. iTerm2's sequence;
            // terminals that do not know it ignore it.
            write_raw(b"\x1b]1337;RequestAttention=no\x07");
            app.attention_asked = false;
        }
        let working = app
            .board
            .rows(now_ms())
            .iter()
            .any(|s| s.state == State::Working);
        if !event::poll(if working { SPINNER_FRAME } else { TICK })? {
            continue;
        }
        match event::read()? {
            // With the key list open, any key just closes it.
            TermEvent::Key(key) if key.kind == KeyEventKind::Press && app.show_keys => {
                app.show_keys = false
            }
            TermEvent::Key(key) if key.kind == KeyEventKind::Press => {
                let ctrl_c =
                    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    _ if ctrl_c => return Ok(()),
                    // Not Esc: the dashboard stays open all day, and Esc is
                    // pressed out of habit to leave whatever has the keyboard.
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Char('j') | KeyCode::Down => move_selection(app, 1),
                    KeyCode::Char('k') | KeyCode::Up => move_selection(app, -1),
                    KeyCode::Char('g') | KeyCode::Home => move_selection(app, isize::MIN / 2),
                    KeyCode::Char('G') | KeyCode::End => move_selection(app, isize::MAX / 2),
                    KeyCode::Tab => next_wanting(app, true),
                    KeyCode::BackTab => next_wanting(app, false),
                    KeyCode::Char('A') => {
                        let marks = app.board.acknowledge_all();
                        app.status = Some((format!("marked {} seen", marks.len()), false));
                        for mark in marks {
                            keep(app, Some(mark));
                        }
                    }
                    KeyCode::Char('m') => {
                        app.settings.bell = app.settings.bell.next();
                        save_settings(app, app.settings.bell.label().to_string());
                    }
                    KeyCode::Enter => open_selected(app),
                    KeyCode::Char(' ') => {
                        if let Some(key) = app.selected.clone() {
                            let mark = app.board.acknowledge(&key);
                            keep(app, mark);
                        }
                    }
                    KeyCode::Char('?') => app.show_keys = true,
                    KeyCode::Char('e') => {
                        app.settings.events = app.settings.events.next();
                        let shown = app.settings.events.label();
                        app.status = Some(match app.settings.save() {
                            Ok(()) => (shown.to_string(), false),
                            Err(e) => (format!("{shown}, but could not save it: {e}"), true),
                        });
                    }
                    KeyCode::Char('i') => {
                        app.settings.icons = !app.settings.icons;
                        let shown = if app.settings.icons {
                            "icons on (needs Nerd Fonts 3.5 or later)"
                        } else {
                            "icons off"
                        };
                        app.status = Some(match app.settings.save() {
                            Ok(()) => (shown.to_string(), false),
                            Err(e) => (format!("{shown}, but could not save it: {e}"), true),
                        });
                    }
                    KeyCode::Char('s') => {
                        app.settings.sort = match app.settings.sort {
                            Order::Fixed => Order::Urgency,
                            Order::Urgency => Order::Fixed,
                        };
                        let label = sort_label(app.settings.sort);
                        app.status = Some(match app.settings.save() {
                            Ok(()) => (format!("sorted {label}"), false),
                            Err(e) => (format!("sorted {label}, but could not save it: {e}"), true),
                        });
                    }
                    KeyCode::Char('d') => {
                        if let Some(key) = app.selected.take() {
                            let mark = app.board.dismiss(&key);
                            keep(app, mark);
                        }
                    }
                    _ => {}
                }
            }
            // Mouse-up, not mouse-down: acting on the press lets Obsidian take
            // the focus back on release (spike/FINDINGS.md).
            TermEvent::Mouse(mouse) if mouse.kind == MouseEventKind::Up(MouseButton::Left) => {
                if let Some((index, on_folder)) = hit(app, mouse.column, mouse.row) {
                    app.selected = app.visible.get(index).cloned();
                    // Only the folder name is a link. A click anywhere else
                    // just selects, so clicking into the pane to give it the
                    // keyboard never switches windows.
                    if on_folder {
                        open_selected(app);
                    }
                }
            }
            TermEvent::Mouse(mouse) if mouse.kind == MouseEventKind::Moved => {
                let hover = hit(app, mouse.column, mouse.row)
                    .filter(|&(_, on_folder)| on_folder)
                    .map(|(i, _)| i);
                if hover != app.hover {
                    // The last message was about something else; make room for the hint.
                    if hover.is_some() {
                        app.status = None;
                    }
                    app.hover = hover;
                }
            }
            TermEvent::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => {
                move_selection(app, 1)
            }
            TermEvent::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => {
                move_selection(app, -1)
            }
            _ => {}
        }
    }
}

/// The row under a screen position, and whether the position is on that
/// row's folder link.
fn hit(app: &App, column: u16, row: u16) -> Option<(usize, bool)> {
    let area = app.rows_area;
    let inside = column >= area.x
        && column < area.x + area.width
        && row >= area.y
        && row < area.y + area.height;
    if !inside {
        return None;
    }
    let index = (row - area.y) as usize + app.list_offset;
    let width = *app.visible_folder_widths.get(index)?;
    let column = column - area.x;
    let start = folder_column(app.settings.icons);
    Some((index, column >= start && column < start + width as u16))
}

/// How the window a session opens in is named in the hint.
fn host_name(exe: &str) -> String {
    match exe.to_lowercase().as_str() {
        "code" => "VS Code".into(),
        "code - insiders" => "VS Code Insiders".into(),
        "windowsterminal" => "Windows Terminal".into(),
        "wezterm-gui" => "WezTerm".into(),
        "obsidian" => "Obsidian".into(),
        _ => exe.to_string(),
    }
}

/// The app a session runs in, short enough for its own column. Several
/// sessions often share a folder; this tells them apart at a glance.
fn host_short(exe: Option<&str>) -> String {
    let Some(exe) = exe else { return "-".into() };
    match exe.to_lowercase().as_str() {
        "code" | "code - insiders" => "code".into(),
        "windowsterminal" => "wt".into(),
        "wezterm-gui" => "wez".into(),
        "obsidian" => "obs".into(),
        "cursor" => "cur".into(),
        other => other.chars().take(APP_WIDTH).collect(),
    }
}

/// Saves the settings and says what changed, or that it could not be kept.
fn save_settings(app: &mut App, shown: String) {
    app.status = Some(match app.settings.save() {
        Ok(()) => (shown, false),
        Err(e) => (format!("{shown}, but could not save it: {e}"), true),
    });
}

/// Selects the next (or previous) row that waits or has an unseen finish,
/// going round the list.
fn next_wanting(app: &mut App, forward: bool) {
    let rows = app.board.rows_in(now_ms(), app.settings.sort);
    let count = rows.len();
    let current = app
        .selected
        .as_ref()
        .and_then(|k| rows.iter().position(|s| s.key() == *k));
    let found = (1..=count)
        .map(|step| match (current, forward) {
            (Some(i), true) => (i + step) % count,
            (Some(i), false) => (i + count - step % count) % count,
            (None, true) => step - 1,
            (None, false) => count - step,
        })
        .find(|&i| rows[i].wants_attention());
    match found {
        Some(i) => app.selected = Some(rows[i].key()),
        None => app.status = Some(("nothing waits or is unseen".to_string(), false)),
    }
}

fn move_selection(app: &mut App, step: isize) {
    if app.visible.is_empty() {
        return;
    }
    let current = app
        .selected
        .as_ref()
        .and_then(|k| app.visible.iter().position(|v| v == k));
    let next = match current {
        None => 0,
        Some(i) => (i as isize)
            .saturating_add(step)
            .clamp(0, app.visible.len() as isize - 1) as usize,
    };
    app.selected = Some(app.visible[next].clone());
}

/// Clears a status message once it has been up long enough. A message
/// counts from when it first appeared; one still being worked on
/// ("opening…") stays.
fn fade_status(app: &mut App) {
    let now = std::time::Instant::now();
    match (&app.status, &app.status_seen) {
        (None, _) => app.status_seen = None,
        (Some(current), Some((seen, _))) if current == seen => {}
        (Some(current), _) => app.status_seen = Some((current.clone(), now)),
    }
    if let Some(((_, is_error), since)) = &app.status_seen {
        let limit = STATUS_SECS * if *is_error { 3 } else { 1 };
        if app.opening.is_none() && now.duration_since(*since).as_secs() >= limit {
            app.status = None;
            app.status_seen = None;
        }
    }
}

/// Starts bringing the selected session's window (and tab) forward.
///
/// That can take a second or two (a helper process, `wezterm cli`, waiting to
/// see the focus stick), so it runs on its own thread. Done on this one, the
/// dashboard froze, and the pointer moves and clicks made meanwhile were all
/// handled at once afterwards: the selection jumped, clicks and hover were lost.
fn open_selected(app: &mut App) {
    if app.opening.is_some() {
        app.open_next = true;
        return;
    }
    let Some(key) = app.selected.clone() else {
        return;
    };
    let now = now_ms();
    let Some(session) = app
        .board
        .rows(now)
        .into_iter()
        .find(|s| s.key() == key)
        .cloned()
    else {
        return;
    };
    let mark = app.board.acknowledge(&key);
    keep(app, mark);
    let target = host::Target {
        agent_exe: session.agent.exe().to_string(),
        host: session
            .host_pid
            .zip(session.host_exe.clone())
            .map(|(pid, exe)| host::Host {
                pid,
                exe,
                agent_pid: session.agent_pid,
            }),
        pane: session.pane.clone(),
        cwd: session.cwd.clone(),
    };
    let label = format!("{} ({})", session.folder(), session.agent.name());
    let ended = session.state == State::Ended;
    app.status = Some((format!("opening {label}…"), false));
    let (tx, rx) = mpsc::channel();
    app.opening = Some(rx);
    std::thread::spawn(move || {
        let status = match host::focus_session(&target) {
            Ok(None) => (format!("opened {label}"), false),
            Ok(Some(tab)) => (format!("opened {label} · {tab}"), false),
            // An ended session's window is usually closed too: not a fault.
            Err(_) if ended => (format!("{label} has ended and its window is gone"), false),
            Err(e) => (e, true),
        };
        let _ = tx.send(status);
    });
}

/// Stores a mark so a restart remembers it; a failure only costs that.
fn keep(app: &mut App, mark: Option<Mark>) {
    if let Some(mark) = mark {
        if let Err(e) = store::append_line(&store::marks_path(), &mark) {
            app.status = Some((format!("could not remember that: {e}"), true));
        }
    }
}

fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

fn state_cell(session: &Session, frame: usize) -> Span<'static> {
    match session.state {
        State::Waiting => Span::styled(
            "● wait ",
            Style::new().fg(WAIT_COLOR).add_modifier(Modifier::BOLD),
        ),
        State::Done if !session.seen => Span::styled(
            "✓ done ",
            Style::new().fg(DONE_COLOR).add_modifier(Modifier::BOLD),
        ),
        State::Done => Span::styled("✓ done ", Style::new().add_modifier(Modifier::DIM)),
        State::Working => Span::raw(format!("{} work ", SPINNER[frame % SPINNER.len()])),
        State::Idle => Span::styled("  idle ", Style::new().add_modifier(Modifier::DIM)),
        State::Ended => Span::styled("  ended", Style::new().add_modifier(Modifier::DIM)),
    }
}

/// Cuts `text` to at most `width` terminal columns.
fn clip_width(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out
}

/// Fits `text` into `width` terminal columns plus one space. Counts display
/// width, so CJK folder names line up with ASCII ones.
fn pad(text: &str, width: usize) -> String {
    let mut out = clip_width(text, width);
    let used = display_width(&out);
    out.extend(std::iter::repeat_n(' ', width - used + 1));
    out
}

/// The session box title, with a legend for the timeline when it is shown.
fn sort_label(order: Order) -> &'static str {
    match order {
        Order::Fixed => "in order of arrival",
        Order::Urgency => "by urgency",
    }
}

fn sessions_title(timeline: usize, order: Order) -> Line<'static> {
    let sorted = if order == Order::Urgency {
        " · by urgency"
    } else {
        ""
    };
    if timeline == 0 {
        return Line::raw(format!(" sessions{sorted} "));
    }
    let dim = Style::new().add_modifier(Modifier::DIM);
    Line::from(vec![
        Span::raw(format!(" sessions{sorted} · last {timeline} min   ")),
        Span::styled("━", Style::new().fg(agent_color(Agent::Claude))),
        Span::styled(" working  ", dim),
        Span::styled(
            "━",
            Style::new().fg(WAIT_COLOR).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" waiting  ", dim),
        Span::styled(
            "✓",
            Style::new().fg(DONE_COLOR).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" finished  ", dim),
        Span::styled("·", dim),
        Span::styled(" idle ", dim),
    ])
}

fn timeline_spans(session: &Session, now: u64, slots: usize) -> Vec<Span<'static>> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let mut spans: Vec<Span> = session
        .timeline(now, slots, SLOT_MS)
        .into_iter()
        .map(|slot| match slot {
            Slot::None => Span::raw(" "),
            Slot::Idle => Span::styled("·", dim),
            Slot::Working => Span::styled("━", Style::new().fg(agent_color(session.agent))),
            Slot::Waiting => Span::styled(
                "━",
                Style::new().fg(WAIT_COLOR).add_modifier(Modifier::BOLD),
            ),
            Slot::Finished => Span::styled(
                "✓",
                Style::new().fg(DONE_COLOR).add_modifier(Modifier::BOLD),
            ),
        })
        .collect();
    spans.push(Span::raw("  "));
    spans
}

/// Nerd Fonts glyphs (codicons). Claude's and OpenAI's arrived in Nerd Fonts
/// 3.5.0 (absent from 3.4.0), Copilot's earlier; an older font shows boxes for
/// the first two.
fn agent_icon(agent: Agent) -> char {
    match agent {
        Agent::Claude => '\u{ec82}',  // cod-claude
        Agent::Copilot => '\u{ec1e}', // cod-copilot
        Agent::Codex => '\u{ec81}',   // cod-openai
    }
}

/// An agent's name, padded to its column, with its icon when icons are on.
fn agent_cell(agent: Agent, icons: bool) -> Span<'static> {
    let name = pad(agent.name(), AGENT_WIDTH);
    let text = if icons {
        format!("{} {name}", agent_icon(agent))
    } else {
        name
    };
    Span::styled(text, Style::new().fg(agent_color(agent)))
}

/// How long a finish counts as just now: its age stands out until then.
const FRESH_MS: u64 = 5 * 60 * 1000;

/// When the newest finish not yet seen happened, if there is one.
fn newest_unseen_finish(rows: &[&Session]) -> Option<u64> {
    rows.iter()
        .filter(|s| s.state == State::Done && !s.seen)
        .map(|s| s.since)
        .max()
}

/// The age column is dim, except for a finish not yet seen that is the
/// newest one or under `FRESH_MS` old: the eye finds what to look at next
/// without the rows moving, however long ago that was.
fn age_style(session: &Session, now: u64, newest: Option<u64>) -> Style {
    let fresh = session.state == State::Done
        && !session.seen
        && (newest == Some(session.since) || now.saturating_sub(session.since) < FRESH_MS);
    if fresh {
        Style::new().fg(DONE_COLOR).add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::DIM)
    }
}

fn session_line(
    session: &Session,
    now: u64,
    newest: Option<u64>,
    frame: usize,
    timeline: usize,
    hovered: bool,
    icons: bool,
) -> Line<'static> {
    let link = if hovered {
        Style::new()
            .fg(LINK_COLOR)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    };
    let dim = Style::new().add_modifier(Modifier::DIM);
    let mut spans =
        vec![
            state_cell(session, frame),
            Span::raw(" "),
            agent_cell(session.agent, icons),
            Span::styled(
                pad(&host_short(session.host_exe.as_deref()), APP_WIDTH),
                Style::new().add_modifier(Modifier::DIM),
            ),
            // Underlined: the folder name is the link that opens the window.
            Span::styled(clip_width(session.folder(), FOLDER_WIDTH), link),
            Span::raw(" ".repeat(
                FOLDER_WIDTH - display_width(&clip_width(session.folder(), FOLDER_WIDTH)) + 1,
            )),
            Span::styled(
                format!("{:>4}  ", age(now, session.since)),
                age_style(session, now, newest),
            ),
        ];
    if timeline > 0 {
        spans.extend(timeline_spans(session, now, timeline));
    }
    match (session.state, &session.detail, &session.prompt) {
        (State::Waiting | State::Done, Some(detail), _) => spans.push(Span::raw(plain(detail))),
        (_, _, Some(prompt)) => spans.push(Span::styled(format!("> {}", plain(prompt)), dim)),
        _ => {}
    }
    Line::from(spans)
}

fn header(app: &App, now: u64) -> Line<'static> {
    let rows = app.board.rows(now);
    let waiting = rows.iter().filter(|s| s.state == State::Waiting).count();
    let done = rows
        .iter()
        .filter(|s| s.state == State::Done && !s.seen)
        .count();
    let working = rows.iter().filter(|s| s.state == State::Working).count();

    let mut spans = Vec::new();
    let mut count = |n: usize, label: &str, color: Option<Color>| {
        let style = match (n, color) {
            (0, _) => Style::new().add_modifier(Modifier::DIM),
            (_, Some(c)) => Style::new().fg(c).add_modifier(Modifier::BOLD),
            (_, None) => Style::new(),
        };
        spans.push(Span::styled(format!(" {n} {label} "), style));
    };
    count(waiting, "waiting", Some(WAIT_COLOR));
    count(done, "done", Some(DONE_COLOR));
    count(working, "working", None);
    spans.push(Span::raw("   "));
    // An agent that has never reported usually has untrusted or missing hooks
    // (DESIGN.md §8), so "never" is shown rather than left out. Agents without
    // our hooks are not listed at all, unless events from them are on file.
    for agent in Agent::ALL
        .into_iter()
        .filter(|a| app.agents.contains(a) || app.board.last_seen(*a).is_some())
    {
        let label = if app.settings.icons {
            format!("{} {}", agent_icon(agent), agent.name())
        } else {
            agent.name().to_string()
        };
        spans.push(Span::styled(label, Style::new().fg(agent_color(agent))));
        match app.board.last_seen(agent) {
            Some(at) => spans.push(Span::styled(
                format!(" {}  ", age(now, at)),
                Style::new().add_modifier(Modifier::DIM),
            )),
            None => spans.push(Span::styled(" never  ", Style::new().fg(WAIT_COLOR))),
        }
    }
    Line::from(spans)
}

/// The log's label for an event: a prompt the agent's tooling submitted is
/// a notice, not something the person asked.
fn event_label(e: &bellpanel::event::Event) -> &'static str {
    if e.automatic {
        "notice"
    } else {
        kind_label(e.kind)
    }
}

fn kind_label(kind: Kind) -> &'static str {
    match kind {
        Kind::Start => "start",
        Kind::Prompt => "prompt",
        Kind::Waiting => "waiting",
        Kind::Resumed => "approved",
        Kind::Stop => "finished",
        Kind::End => "ended",
    }
}

/// The event log's lines and title. By default it follows the selected
/// session, reading like that session's conversation: what was asked, what
/// came back. `e` switches to each session's latest event, then to every
/// session's events in one stream.
fn event_log(
    app: &App,
    now: u64,
    rows: usize,
    width: usize,
) -> (Vec<Line<'static>>, Line<'static>) {
    let dim = Style::new().add_modifier(Modifier::DIM);
    // Older records still carry Codex's uninformative end reason "other".
    let text_of = |e: &bellpanel::event::Event| {
        e.text
            .as_deref()
            .filter(|t| !(e.kind == Kind::End && *t == "other"))
            .map(plain)
            .unwrap_or_default()
    };
    let selected = app
        .selected
        .as_ref()
        .filter(|_| app.settings.events == EventsMode::Selected);
    if app.settings.events == EventsMode::Latest {
        return latest_events(app, now, rows, width, &text_of);
    }
    let mut lines: Vec<Line> = match selected {
        Some(key) => app
            .board
            .session_log(key)
            .rev()
            .take(rows)
            .map(|e| {
                // The person's words stand out; replies and the rest are quieter.
                let text = match e.kind {
                    Kind::Prompt if e.automatic => Span::styled(text_of(e), dim),
                    Kind::Prompt => {
                        Span::styled(text_of(e), Style::new().add_modifier(Modifier::BOLD))
                    }
                    Kind::Waiting => Span::styled(text_of(e), Style::new().fg(WAIT_COLOR)),
                    _ => Span::raw(text_of(e)),
                };
                Line::from(vec![
                    Span::styled(format!("{:>4} ", age(now, e.at)), dim),
                    Span::styled(pad(event_label(e), 9), dim),
                    text,
                ])
            })
            .collect(),
        None => app
            .board
            .log()
            .rev()
            .take(rows)
            .map(|e| {
                Line::from(vec![
                    Span::styled(format!("{:>4} ", age(now, e.at)), dim),
                    agent_cell(e.agent, app.settings.icons),
                    Span::raw(pad(bellpanel::state::folder_of(&e.cwd), 20)),
                    Span::raw(pad(event_label(e), 9)),
                    Span::styled(text_of(e), dim),
                ])
            })
            .collect(),
    };
    lines.reverse();
    let title = match selected {
        Some((agent, id)) => {
            let folder = app
                .board
                .rows(now)
                .into_iter()
                .find(|s| s.agent == *agent && s.id == *id)
                .map(|s| s.folder().to_string())
                .unwrap_or_default();
            Line::from(vec![
                Span::raw(" events · "),
                Span::styled(folder, Style::new().add_modifier(Modifier::BOLD)),
                Span::raw(" "),
                Span::styled(agent.name(), Style::new().fg(agent_color(*agent))),
                Span::styled("  (e: each session's latest) ", dim),
            ])
        }
        None => Line::from(vec![
            Span::raw(" events · every session "),
            Span::styled(" (e: off) ", dim),
        ]),
    };
    (lines, title)
}

/// Each listed session's latest prompt, wait, approval or finish, in the
/// list's order. The log's height is shared out between them and each text
/// wraps into its share, so a few sessions show their whole reply and many
/// still fit one line each.
fn latest_events(
    app: &App,
    now: u64,
    rows: usize,
    width: usize,
    text_of: &dyn Fn(&bellpanel::event::Event) -> String,
) -> (Vec<Line<'static>>, Line<'static>) {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let latest: Vec<_> = app
        .board
        .rows_in(now, app.settings.sort)
        .into_iter()
        .filter_map(|s| app.board.session_log(&s.key()).next_back().map(|e| (s, e)))
        .take(rows)
        .collect();
    let indent = 5;
    let text_width = width.saturating_sub(indent).max(10);
    let entries: Vec<_> = latest
        .into_iter()
        .map(|(s, e)| {
            let head = vec![
                Span::styled(format!("{:>4} ", age(now, e.at)), dim),
                agent_cell(s.agent, app.settings.icons),
                Span::raw(pad(s.folder(), 20)),
                Span::styled(event_label(e), dim),
            ];
            // A finish without a text (Copilot's) says nothing; what was
            // asked tells what the session did.
            let (text, style) = match (text_of(e), e.kind, &s.prompt) {
                (t, Kind::Stop, Some(prompt)) if t.is_empty() => {
                    (format!("> {}", plain(prompt)), dim)
                }
                (t, Kind::Prompt, _) if e.automatic => (t, dim),
                (t, Kind::Prompt, _) => (t, Style::new().add_modifier(Modifier::BOLD)),
                (t, Kind::Waiting, _) => (t, Style::new().fg(WAIT_COLOR)),
                (t, _, _) => (t, Style::new()),
            };
            (head, text, style)
        })
        .collect();
    // Lines each would take in full: its head, then its wrapped text.
    let needs: Vec<usize> = entries
        .iter()
        .map(|(_, text, _)| 1 + wrap_width(text, text_width, usize::MAX).len())
        .collect();
    let mut lines = Vec::new();
    for ((head, text, style), share) in entries.into_iter().zip(share_rows(&needs, rows)) {
        if share < 2 {
            // No room for a second line: the text follows on the same one.
            let mut line = head;
            line.push(Span::raw("  "));
            line.push(Span::styled(text, style));
            lines.push(Line::from(line));
            continue;
        }
        // The text gets lines of its own, the pane's full width: beside the
        // head it would be squeezed into a narrow column.
        lines.push(Line::from(head));
        for part in wrap_width(&text, text_width, share - 1) {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(part, style),
            ]));
        }
    }
    let title = Line::from(vec![
        Span::raw(" events · each session's latest "),
        Span::styled(" (e: every event) ", dim),
    ]);
    (lines, title)
}

/// `rows` lines shared out between entries that would take `needs` lines
/// each: short ones get all they need, and what they leave goes to the long
/// ones, evenly. Every entry gets at least one line.
fn share_rows(needs: &[usize], rows: usize) -> Vec<usize> {
    if needs.iter().sum::<usize>() <= rows {
        return needs.to_vec();
    }
    // The largest cap that fits, so no entry takes more than it while
    // another is cut shorter.
    let fits = |cap: usize| needs.iter().map(|&n| n.min(cap)).sum::<usize>() <= rows;
    let mut cap = 1;
    while fits(cap + 1) {
        cap += 1;
    }
    let mut shares: Vec<usize> = needs.iter().map(|&n| n.min(cap)).collect();
    // Lines left over go one each to the first entries that want more.
    let mut left = rows.saturating_sub(shares.iter().sum());
    for (share, &need) in shares.iter_mut().zip(needs) {
        if left == 0 {
            break;
        }
        if need > *share {
            *share += 1;
            left -= 1;
        }
    }
    shares
}

/// Characters a Japanese line does not start with (kinsoku).
const NO_LINE_START: &str = "、。，．,.)）」』】〕〉》？！?!ー…・";

/// `text` cut into at most `max_lines` pieces of `width` columns each. A
/// closing mark that would start a line takes the previous character down
/// with it, and text left over is marked with `…`.
fn wrap_width(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let cols = |s: &str| display_width(s);
    let mut lines = vec![String::new()];
    let mut cut = false;
    for c in text.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        let full = lines.len() == max_lines;
        let line = lines.last_mut().unwrap();
        if cols(line) + w > width {
            if full {
                cut = true;
                break;
            }
            let mut next = String::new();
            if NO_LINE_START.contains(c) && line.chars().count() > 1 {
                next.push(line.pop().unwrap());
            }
            lines.push(next);
        }
        let line = lines.last_mut().unwrap();
        // A space where the text was broken would indent the next line.
        if line.is_empty() && c == ' ' {
            continue;
        }
        line.push(c);
    }
    if cut {
        let last = lines.last_mut().unwrap();
        while !last.is_empty() && cols(last) + 1 > width {
            last.pop();
        }
        last.push('…');
    }
    lines
}

/// Heights of the session box and the event log in a terminal `height` rows
/// tall. Sessions come first and take what they need; the log gets what is
/// left, and is left out when that is less than one line inside its border.
fn split_heights(height: u16, sessions: u16) -> (u16, u16) {
    let room = height.saturating_sub(2); // header and footer
    let list = (sessions.max(1) + 2).min(room);
    let log = room - list;
    if log < 3 {
        (room, 0)
    } else {
        (list, log)
    }
}

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let now = now_ms();
    let area = frame.area();
    let (list_height, log_height) = if app.settings.events == EventsMode::Off {
        (area.height.saturating_sub(2), 0)
    } else {
        split_heights(area.height, app.board.rows(now).len() as u16)
    };
    let [head_area, list_area, log_area, foot_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(list_height),
        Constraint::Length(log_height),
        Constraint::Length(1),
    ])
    .areas(area);
    let timeline = timeline_slots(area.width, app.settings.icons);

    frame.render_widget(Paragraph::new(header(app, now)), head_area);

    let rows = app.board.rows_in(now, app.settings.sort);
    app.visible = rows.iter().map(|s| s.key()).collect();
    app.visible_folder_widths = rows
        .iter()
        .map(|s| display_width(s.folder()).min(FOLDER_WIDTH))
        .collect();
    // A selection whose row has gone moves to the row now in its place,
    // not back to the top.
    if app
        .selected
        .as_ref()
        .is_none_or(|k| !app.visible.contains(k))
    {
        let at = app.selected_at.min(app.visible.len().saturating_sub(1));
        app.selected = app.visible.get(at).cloned();
    }
    let selected_index = app
        .selected
        .as_ref()
        .and_then(|k| app.visible.iter().position(|v| v == k));
    app.selected_at = selected_index.unwrap_or(0);
    let newest = newest_unseen_finish(&rows);
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, s)| {
            ListItem::new(session_line(
                s,
                now,
                newest,
                (app.started.elapsed().as_millis() / SPINNER_FRAME.as_millis()) as usize,
                timeline,
                app.hover == Some(i),
                app.settings.icons,
            ))
        })
        .collect();
    let empty = items.is_empty();
    let list = List::new(items)
        .block(Block::bordered().title(sessions_title(timeline, app.settings.sort)))
        // A tinted background, not reversed colours: reversing turns every
        // coloured timeline cell into a coloured block.
        .highlight_style(Style::new().bg(SELECTED_BG));
    let mut state = ListState::default()
        .with_selected(selected_index)
        .with_offset(app.list_offset);
    frame.render_stateful_widget(list, list_area, &mut state);
    app.list_offset = state.offset();
    app.rows_area = Block::bordered().inner(list_area);
    if empty {
        let hint = Paragraph::new(format!(
            "No sessions yet. Run `bellpanel install`, restart your agents, and they appear here.\nEvents: {}",
            app.tail.path().display()
        ))
        .style(Style::new().add_modifier(Modifier::DIM));
        frame.render_widget(hint, app.rows_area);
    }

    if log_area.height > 0 {
        let (lines, title) = event_log(
            app,
            now,
            log_area.height.saturating_sub(2) as usize,
            log_area.width.saturating_sub(2) as usize,
        );
        frame.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(title)),
            log_area,
        );
    }

    let hovered = app.hover.and_then(|i| rows.get(i));
    let foot = match (&app.status, hovered) {
        (Some((text, true)), _) => Line::styled(format!(" {text}"), Style::new().fg(ERROR_COLOR)),
        (Some((text, false)), _) => Line::raw(format!(" {text}")),
        (None, Some(s)) => {
            let target = s
                .host_exe
                .as_deref()
                .map_or("its window".to_string(), host_name);
            Line::from(vec![
                Span::raw(" click to open "),
                Span::styled(
                    s.folder().to_string(),
                    Style::new().fg(LINK_COLOR).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(" in {target}")),
            ])
        }
        // On a finish not yet seen, the keys that deal with it.
        (None, None)
            if app
                .selected
                .as_ref()
                .and_then(|k| rows.iter().find(|s| s.key() == *k))
                .is_some_and(|s| s.state == State::Done && !s.seen) =>
        {
            Line::styled(
                format!(" space: mark seen · d: hide · {HELP}"),
                Style::new().add_modifier(Modifier::DIM),
            )
        }
        (None, None) => Line::styled(format!(" {HELP}"), Style::new().add_modifier(Modifier::DIM)),
    };
    frame.render_widget(Paragraph::new(foot), foot_area);

    if app.show_keys {
        draw_keys(frame, area);
    }
}

/// The `?` key list, over the middle of the screen.
fn draw_keys(frame: &mut ratatui::Frame, area: Rect) {
    let key_width = KEYS
        .iter()
        .map(|(k, _)| display_width(k))
        .max()
        .unwrap_or(0);
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(
                    pad(key, key_width),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::raw(*what),
            ])
        })
        .collect();
    let width = (lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16 + 4).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" keys · any key closes ")
                .padding(ratatui::widgets::Padding::horizontal(1)),
        ),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(session: &str, kind: Kind, at: u64, text: &str) -> bellpanel::event::Event {
        bellpanel::event::Event {
            v: bellpanel::event::FORMAT_VERSION,
            at,
            agent: Agent::Claude,
            kind,
            event: String::new(),
            session: session.into(),
            turn: None,
            cwd: format!("C:\\work\\{session}"),
            host_pid: None,
            host_exe: None,
            agent_pid: None,
            pane: None,
            text: (!text.is_empty()).then(|| text.to_string()),
            automatic: false,
        }
    }

    fn app_with(events: &[bellpanel::event::Event]) -> App {
        let mut app = App {
            settings: Settings::default(),
            agents: Vec::new(),
            board: Board::default(),
            tail: Tail::open(std::env::temp_dir().join("bellpanel-test-none.jsonl")),
            selected: None,
            visible: Vec::new(),
            visible_folder_widths: Vec::new(),
            rows_area: Rect::default(),
            list_offset: 0,
            status: None,
            status_seen: None,
            show_keys: false,
            hover: None,
            opening: None,
            open_next: false,
            selected_at: 0,
            attention_asked: false,
            commands: CommandWatch::default(),
            watched_at: std::time::Instant::now(),
            started: std::time::Instant::now(),
        };
        for e in events {
            app.board.apply(e);
        }
        app
    }

    /// The screen as text, one string per row.
    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_selection_stays_in_place_when_its_row_goes() {
        let now = now_ms();
        let events: Vec<_> = (0..3)
            .map(|i| event(&format!("s{i}"), Kind::Prompt, now - 9_000 + i, "x"))
            .collect();
        let mut app = app_with(&events);
        screen(&mut app, 100, 12);
        move_selection(&mut app, 1);
        screen(&mut app, 100, 12);
        let gone = app.selected.clone().unwrap();
        assert_eq!(gone.1, "s1");
        app.board.dismiss(&gone);
        screen(&mut app, 100, 12);
        assert_eq!(app.selected.as_ref().unwrap().1, "s2");
    }

    #[test]
    fn tab_goes_round_the_rows_that_want_the_user() {
        let now = now_ms();
        let mut app = app_with(&[
            event("a", Kind::Prompt, now - 9_000, "x"),
            event("b", Kind::Prompt, now - 8_000, "x"),
            event("c", Kind::Prompt, now - 7_000, "x"),
            event("b", Kind::Waiting, now - 6_000, "Bash: ls"),
            event("c", Kind::Stop, now - 5_000, "done"),
        ]);
        screen(&mut app, 100, 12);
        let picked = |app: &App| app.selected.as_ref().unwrap().1.clone();
        next_wanting(&mut app, true);
        assert_eq!(picked(&app), "b");
        next_wanting(&mut app, true);
        assert_eq!(picked(&app), "c");
        next_wanting(&mut app, true);
        assert_eq!(picked(&app), "b");
        next_wanting(&mut app, false);
        assert_eq!(picked(&app), "c");
    }

    #[test]
    fn the_bell_rings_for_what_it_is_set_to() {
        assert!(Bell::Both.rings(Kind::Stop) && Bell::Both.rings(Kind::Waiting));
        assert!(Bell::Waits.rings(Kind::Waiting) && !Bell::Waits.rings(Kind::Stop));
        assert!(!Bell::Off.rings(Kind::Waiting));
        assert_eq!(Bell::Both.next().next().next(), Bell::Both);
    }

    #[test]
    fn the_timeline_shrinks_with_the_terminal() {
        assert_eq!(timeline_slots(130, false), TIMELINE_SLOTS);
        assert_eq!(
            timeline_slots(80, false),
            80 - ROW_FIXED_WIDTH - 2 - ROW_TEXT_MIN_WIDTH
        );
        assert_eq!(
            timeline_slots(80, true),
            80 - ROW_FIXED_WIDTH - ICON_WIDTH - 2 - ROW_TEXT_MIN_WIDTH
        );
        assert_eq!(timeline_slots(60, false), 0);
    }

    #[test]
    fn sessions_get_their_rows_before_the_log() {
        // A short pane, like a terminal docked under a note: 3 sessions, 8 rows.
        assert_eq!(split_heights(8, 3), (6, 0));
        // Room for both: sessions take 3 + borders, the log the rest.
        assert_eq!(split_heights(30, 3), (5, 23));
        // Too little for one log line: sessions keep everything.
        assert_eq!(split_heights(9, 3), (7, 0));
        assert_eq!(split_heights(10, 3), (5, 3));
        // More sessions than fit: the box fills the pane.
        assert_eq!(split_heights(12, 40), (10, 0));
    }

    #[test]
    fn apps_get_short_names() {
        assert_eq!(host_short(Some("Code")), "code");
        assert_eq!(host_short(Some("WindowsTerminal")), "wt");
        assert_eq!(host_short(Some("wezterm-gui")), "wez");
        assert_eq!(host_short(Some("Obsidian")), "obs");
        assert_eq!(host_short(Some("alacritty")), "alac");
        assert_eq!(host_short(None), "-");
    }

    #[test]
    fn the_events_key_cycles_through_four_views() {
        let mut mode = EventsMode::default();
        let mut seen = vec![mode];
        for _ in 0..4 {
            mode = mode.next();
            seen.push(mode);
        }
        assert_eq!(
            seen,
            [
                EventsMode::Selected,
                EventsMode::Latest,
                EventsMode::All,
                EventsMode::Off,
                EventsMode::Selected
            ]
        );
    }

    #[test]
    fn settings_saved_by_earlier_versions_still_load() {
        let old: Settings =
            serde_json::from_str(r#"{"sort":"urgency","all_events":true,"icons":true}"#).unwrap();
        assert_eq!(
            (old.sort, old.events, old.icons),
            (Order::Urgency, EventsMode::Selected, true)
        );
    }

    #[test]
    fn wrapping_counts_display_width_and_stops_at_the_line_limit() {
        assert_eq!(wrap_width("abcdef", 4, 3), ["abcd", "ef"]);
        assert_eq!(wrap_width("日本語です", 4, 3), ["日本", "語で", "す"]);
        assert_eq!(wrap_width("abcdefghij", 3, 2), ["abc", "de…"]);
        assert_eq!(wrap_width("", 4, 3), [""]);
        // A full stop never starts a line: the character before it moves down too.
        assert_eq!(wrap_width("ありがとう。", 10, 2), ["ありがと", "う。"]);
        // A break at a space does not start the next line with it.
        assert_eq!(wrap_width("abc def", 3, 3), ["abc", "def"]);
    }

    #[test]
    fn short_entries_leave_their_lines_to_long_ones() {
        // Everything fits: each gets what it needs.
        assert_eq!(share_rows(&[3, 2, 2], 10), [3, 2, 2]);
        // The short ones keep theirs; the long one takes the rest.
        assert_eq!(share_rows(&[8, 2, 2], 10), [6, 2, 2]);
        // Two long ones split what is left evenly.
        assert_eq!(share_rows(&[9, 9, 2], 10), [4, 4, 2]);
        // Too little room: one line each.
        assert_eq!(share_rows(&[5, 5, 5], 3), [1, 1, 1]);
    }

    #[test]
    fn the_newest_or_a_fresh_unseen_finish_has_its_age_stand_out() {
        let finish = |session: &str, at: u64| bellpanel::event::Event {
            v: 1,
            at,
            agent: Agent::Claude,
            kind: Kind::Stop,
            event: "Stop".into(),
            session: session.into(),
            turn: None,
            cwd: format!("C:/work/{session}"),
            host_pid: None,
            host_exe: None,
            agent_pid: None,
            pane: None,
            text: None,
            automatic: false,
        };
        let now = 60 * 60 * 1000;
        let mut board = Board::default();
        board.apply(&finish("fresh", now - 60_000));
        board.apply(&finish("old", now - FRESH_MS - 1));
        board.apply(&finish("seen", now - 60_000));
        board.acknowledge(&(Agent::Claude, "seen".into()));
        let bold = |board: &Board, id: &str| {
            let rows = board.rows(now);
            let newest = newest_unseen_finish(&rows);
            let s = rows.iter().find(|s| s.id == id).unwrap();
            age_style(s, now, newest)
                .add_modifier
                .contains(Modifier::BOLD)
        };
        assert!(bold(&board, "fresh"));
        assert!(!bold(&board, "old"));
        assert!(!bold(&board, "seen"));
        // Once the fresh one is seen, the newest left stands out however old.
        board.acknowledge(&(Agent::Claude, "fresh".into()));
        assert!(bold(&board, "old"));
    }

    #[test]
    fn padding_counts_display_width() {
        assert_eq!(pad("日本", 5), "日本  ");
        assert_eq!(pad("abcdef", 3), "abc ");
    }
}
