//! Spike: can a TUI running inside terminal-pane (Obsidian -> pty-host -> conpty)
//! bring another window to the front, and does BEL / OSC 777 mark the tab?
//!
//! Keys: j/k move, Enter or click focuses, m switches focus method,
//! b rings BEL in 5 s, n sends OSC 777 notify in 5 s, r refreshes, q quits.
//! Every attempt is appended to focus-probe.log next to Cargo.toml.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

mod win;

#[derive(Clone, Copy, PartialEq)]
enum Method {
    Plain,
    AttachInput,
}

impl Method {
    fn label(self) -> &'static str {
        match self {
            Method::Plain => "plain SetForegroundWindow",
            Method::AttachInput => "AttachThreadInput + SetForegroundWindow",
        }
    }
}

enum Pending {
    Bell(Instant),
    Notify(Instant),
}

struct App {
    windows: Vec<win::Entry>,
    state: ListState,
    method: Method,
    log: Vec<String>,
    pending: Option<Pending>,
    /// Screen row of the first list item, for mapping clicks.
    list_top: u16,
}

fn main() -> io::Result<()> {
    let mut terminal = ratatui::init();
    execute!(io::stdout(), EnableMouseCapture)?;
    let mut app = App {
        windows: win::list(),
        state: ListState::default().with_selected(Some(0)),
        method: Method::AttachInput,
        log: vec!["click a row or press Enter to focus it".into()],
        pending: None,
        list_top: 1,
    };
    let result = run(&mut terminal, &mut app);
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        fire_pending(app);
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('j') | KeyCode::Down => app.state.select_next(),
                KeyCode::Char('k') | KeyCode::Up => app.state.select_previous(),
                KeyCode::Enter => focus_selected(app, "key"),
                KeyCode::Char('m') => {
                    app.method = match app.method {
                        Method::Plain => Method::AttachInput,
                        Method::AttachInput => Method::Plain,
                    };
                    push(app, format!("method: {}", app.method.label()));
                }
                KeyCode::Char('b') => {
                    app.pending = Some(Pending::Bell(Instant::now() + Duration::from_secs(5)));
                    push(app, "BEL in 5 s: switch to another tab now".into());
                }
                KeyCode::Char('n') => {
                    app.pending = Some(Pending::Notify(Instant::now() + Duration::from_secs(5)));
                    push(
                        app,
                        "OSC 777 notify in 5 s: switch to another tab now".into(),
                    );
                }
                KeyCode::Char('r') => {
                    app.windows = win::list();
                    push(app, format!("{} windows", app.windows.len()));
                }
                _ => {}
            },
            Event::Mouse(mouse)
                if mouse.kind == MouseEventKind::Up(MouseButton::Left)
                    && mouse.row >= app.list_top =>
            {
                let index = (mouse.row - app.list_top) as usize + app.state.offset();
                if index < app.windows.len() {
                    app.state.select(Some(index));
                    focus_selected(app, "click");
                }
            }
            _ => {}
        }
    }
}

fn fire_pending(app: &mut App) {
    let due = match &app.pending {
        Some(Pending::Bell(at)) | Some(Pending::Notify(at)) => Instant::now() >= *at,
        None => false,
    };
    if !due {
        return;
    }
    let mut out = io::stdout();
    match app.pending.take() {
        Some(Pending::Bell(_)) => {
            let _ = out.write_all(b"\x07");
            push(app, "sent BEL".into());
        }
        Some(Pending::Notify(_)) => {
            let _ = out.write_all(b"\x1b]777;notify;focus-probe;agent finished\x07");
            push(app, "sent OSC 777 notify".into());
        }
        None => {}
    }
    let _ = out.flush();
}

fn focus_selected(app: &mut App, via: &str) {
    let Some(entry) = app
        .state
        .selected()
        .and_then(|i| app.windows.get(i))
        .cloned()
    else {
        return;
    };
    let report = win::focus(entry.handle, app.method == Method::AttachInput);
    let mark = |landed: bool| if landed { "YES" } else { "no" };
    std::thread::sleep(Duration::from_millis(300));
    let at_300ms = win::foreground() == entry.handle;
    std::thread::sleep(Duration::from_millis(700));
    let at_1s = win::foreground() == entry.handle;
    push(
        app,
        format!(
            "{via}-up [{}] {} -> SetForegroundWindow={} 300ms={} 1s={} {}",
            app.method.label(),
            entry.title,
            report.returned,
            mark(at_300ms),
            mark(at_1s),
            report.note,
        ),
    );
}

fn push(app: &mut App, line: String) {
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(concat!(env!("CARGO_MANIFEST_DIR"), "/focus-probe.log"))
    {
        let _ = writeln!(file, "{line}");
    }
    app.log.push(line);
}

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let [list_area, log_area, help_area] = Layout::vertical([
        Constraint::Min(5),
        Constraint::Length(8),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    app.list_top = list_area.y + 1;
    let items: Vec<ListItem> = app
        .windows
        .iter()
        .map(|w| ListItem::new(format!("{:<9} {:<16} {}", w.kind, w.app, w.title)))
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(" windows "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, list_area, &mut app.state);

    let tail: Vec<&str> = app
        .log
        .iter()
        .rev()
        .take(log_area.height.saturating_sub(2) as usize)
        .rev()
        .map(String::as_str)
        .collect();
    frame.render_widget(
        Paragraph::new(tail.join("\n")).block(Block::bordered().title(" log ")),
        log_area,
    );
    frame.render_widget(
        Paragraph::new(format!(
            "click/Enter focus  m method ({})  b bell  n notify  r refresh  q quit",
            app.method.label()
        )),
        help_area,
    );
}
