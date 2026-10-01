//! The dashboard: `vx` with no arguments. A table of VMs whose state refreshes every second,
//! with keys for everything the CLI does.
//!
//! Actions run the CLI itself as a child process, so they share its locks, checks and messages:
//! quick ones (start, stop, pause, delete) in the background with their output captured, and
//! interactive ones (ssh, console, logs, new) in the foreground with the dashboard set aside.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};
use std::{env, fmt};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::crossterm::{cursor, execute};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table, TableState,
};
use ratatui::{DefaultTerminal, Frame};

use crate::backend::{self, State};
use crate::style::{self, ERR, OUT};
use crate::vx::{Home, Spec};

const ACCENT: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const BORDER: Style = Style::new().fg(Color::DarkGray);
const HEADER: Style = Style::new().add_modifier(Modifier::BOLD);
const SELECTED: Style = Style::new().bg(Color::Indexed(237)).add_modifier(Modifier::BOLD);
const SELECTED_TEXT: Style = Style::new().fg(Color::White);
const BUSY: Style = Style::new().fg(Color::Cyan);
const OK: Style = Style::new().fg(Color::Green);
const WARN: Style = Style::new().fg(Color::Yellow);
const ERROR: Style = Style::new().fg(Color::Red);

const REFRESH: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(100);
/// How long a message stays in the top border.
const STATUS_FOR: Duration = Duration::from_secs(6);
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// One VM: its spec and state, or why its vx.toml couldn't be read.
#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub vm: Result<(Spec, State), String>,
}

/// Every VM, sorted by name, with its state asked of its backend.
pub fn entries(home: &Home) -> Result<Vec<Entry>> {
    Ok(home
        .names()?
        .into_iter()
        .map(|name| {
            let vm = home.load(&name).map_err(|e| format!("{e:#}")).map(|vm| {
                let state = match backend::get(&vm.spec.backend) {
                    Ok(b) => b.state(&vm),
                    Err(_) => State::Other(format!("unknown backend `{}`", vm.spec.backend)),
                };
                (vm.spec, state)
            });
            Entry { name, vm }
        })
        .collect())
}

/// What the other threads tell the dashboard.
enum Msg {
    Vms(Result<Vec<Entry>, String>),
    /// A background action finished.
    Done { name: String, verb: Verb, result: Result<(), String> },
}

/// Everything the main loop needs besides the app state.
struct Io {
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
    /// Asks the refresher for a refresh now rather than at the next tick.
    wake: Sender<()>,
    exe: PathBuf,
    home: PathBuf,
}

pub fn run(home: &Home) -> Result<()> {
    ignore_sigint();
    let (tx, rx) = mpsc::channel();
    let wake = refresher(home, tx.clone());
    let io = Io { rx, tx, wake, exe: env::current_exe()?, home: home.root().to_path_buf() };
    ratatui::run(|terminal| App::default().run(terminal, &io))
}

/// Ctrl-C in an ssh session or `vx logs -f` should end that, not the dashboard. A no-op handler
/// (rather than SIG_IGN, which children inherit) keeps us alive while children get the default.
fn ignore_sigint() {
    extern "C" fn ignore(_: libc::c_int) {}
    // SAFETY: the handler does nothing, so it's async-signal-safe.
    unsafe { libc::signal(libc::SIGINT, ignore as extern "C" fn(libc::c_int) as libc::sighandler_t) };
}

/// Reload the VM list on a thread, so a slow VM never freezes the screen.
fn refresher(home: &Home, tx: Sender<Msg>) -> Sender<()> {
    let (wake, woken) = mpsc::channel();
    let home = Home::at(home.root());
    thread::spawn(move || {
        loop {
            if tx.send(Msg::Vms(entries(&home).map_err(|e| format!("{e:#}")))).is_err() {
                return; // the dashboard closed
            }
            if let Err(RecvTimeoutError::Disconnected) = woken.recv_timeout(REFRESH) {
                return;
            }
        }
    });
    wake
}

/// Background actions, run as `vx <verb> <name>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Start,
    Stop,
    ForceStop,
    Pause,
    Resume,
    Delete,
}

impl Verb {
    fn args(self) -> &'static [&'static str] {
        match self {
            Verb::Start => &["start"],
            Verb::Stop => &["stop"],
            Verb::ForceStop => &["stop", "--force"],
            Verb::Pause => &["pause"],
            Verb::Resume => &["resume"],
            Verb::Delete => &["rm", "-y"],
        }
    }

    /// Shown in the STATE column while it runs.
    fn doing(self) -> &'static str {
        match self {
            Verb::Start => "starting",
            Verb::Stop | Verb::ForceStop => "stopping",
            Verb::Pause => "pausing",
            Verb::Resume => "resuming",
            Verb::Delete => "deleting",
        }
    }

    fn done(self) -> &'static str {
        match self {
            Verb::Start => "started",
            Verb::Stop | Verb::ForceStop => "stopped",
            Verb::Pause => "paused",
            Verb::Resume => "resumed",
            Verb::Delete => "deleted",
        }
    }
}

impl fmt::Display for Verb {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Verb::Start => "start",
            Verb::Stop | Verb::ForceStop => "stop",
            Verb::Pause => "pause",
            Verb::Resume => "resume",
            Verb::Delete => "delete",
        })
    }
}

/// Interactive actions, which take over the terminal until they end.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Foreground {
    Ssh(String),
    Console(String),
    Logs(String),
    New,
}

impl Foreground {
    fn args(&self) -> Vec<String> {
        let args: &[&str] = match self {
            Foreground::Ssh(name) => &["ssh", name],
            Foreground::Console(name) => &["console", name],
            Foreground::Logs(name) => &["logs", "-f", name],
            Foreground::New => &["new"],
        };
        args.iter().map(|s| s.to_string()).collect()
    }

    /// Whether to hold the screen after it ends, so its output can be read.
    fn hold(&self, status: ExitStatus) -> bool {
        // Ctrl-C ending `logs -f` is how you leave it, not a failure. Anything else that
        // failed has an error worth reading.
        status.code().is_some_and(|code| code != 0)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Quit,
    Job(String, Verb),
    Run(Foreground),
}

#[derive(Debug, PartialEq, Eq)]
enum Modal {
    Help,
    /// Asking before deleting this VM.
    Delete(String),
}

struct Busy {
    verb: Verb,
    /// Finished; cleared once a refresh shows the outcome, so the old state never flashes back.
    done: bool,
}

struct Status {
    text: String,
    style: Style,
    at: Instant,
}

#[derive(Default)]
struct App {
    /// `None` until the first refresh arrives.
    vms: Option<Vec<Entry>>,
    /// Why the last refresh failed, e.g. VX_HOME isn't readable.
    error: Option<String>,
    table: TableState,
    busy: HashMap<String, Busy>,
    status: Option<Status>,
    modal: Option<Modal>,
    /// Advances every tick, for the spinners.
    tick: usize,
}

impl App {
    fn run(mut self, terminal: &mut DefaultTerminal, io: &Io) -> Result<()> {
        loop {
            while let Ok(msg) = io.rx.try_recv() {
                self.handle(msg, io);
            }
            self.tick = self.tick.wrapping_add(1);
            terminal.draw(|frame| {
                self.draw(frame);
                if !OUT.enabled() {
                    style::strip_colors(frame.buffer_mut());
                }
            })?;
            // Wake up regularly so refreshes and spinners move without key presses.
            if !event::poll(TICK)? {
                continue;
            }
            let Event::Key(key) = event::read()? else { continue };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match self.key(key) {
                Action::None => {}
                Action::Quit => return Ok(()),
                Action::Job(name, verb) => spawn_job(io, name, verb),
                Action::Run(fg) => {
                    suspend(terminal, || run_foreground(io, &fg))?;
                    let _ = io.wake.send(());
                }
            }
        }
    }

    fn handle(&mut self, msg: Msg, io: &Io) {
        match msg {
            Msg::Vms(Ok(vms)) => self.update(vms),
            Msg::Vms(Err(e)) => self.error = Some(e),
            Msg::Done { name, verb, result } => {
                match result {
                    Ok(()) => self.say(format!("✓ {name} {}", verb.done()), OK),
                    Err(e) => self.say(format!("✗ couldn't {verb} {name}: {e}"), ERROR),
                }
                if let Some(busy) = self.busy.get_mut(&name) {
                    busy.done = true;
                }
                let _ = io.wake.send(());
            }
        }
    }

    /// Take a fresh list, keeping the same VM selected even if others came or went.
    fn update(&mut self, vms: Vec<Entry>) {
        let selected = self.selected().map(|e| e.name.clone());
        let index = selected.and_then(|name| vms.iter().position(|e| e.name == name));
        let index = match (index, self.table.selected()) {
            (Some(i), _) => Some(i),
            (None, _) if vms.is_empty() => None,
            (None, Some(i)) => Some(i.min(vms.len() - 1)),
            (None, None) => Some(0),
        };
        self.table.select(index);
        self.busy.retain(|_, busy| !busy.done);
        self.vms = Some(vms);
        self.error = None;
    }

    fn selected(&self) -> Option<&Entry> {
        self.vms.as_ref()?.get(self.table.selected()?)
    }

    fn say(&mut self, text: String, style: Style) {
        self.status = Some(Status { text, style, at: Instant::now() });
    }

    /// The selected VM and its state, if it can be acted on; otherwise says why not.
    fn target(&mut self) -> Option<(String, State)> {
        let entry = self.selected()?;
        let name = entry.name.clone();
        let found = match &entry.vm {
            Err(e) => Err(format!("✗ {name}: {e}")),
            Ok(_) if self.busy.contains_key(&name) => {
                Err(format!("{name} is busy {}", self.busy[&name].verb.doing()))
            }
            Ok((_, state)) => Ok(state.clone()),
        };
        match found {
            Ok(state) => Some((name, state)),
            Err(why) => {
                self.say(why, WARN);
                None
            }
        }
    }

    fn job(&mut self, name: String, verb: Verb) -> Action {
        self.busy.insert(name.clone(), Busy { verb, done: false });
        Action::Job(name, verb)
    }

    fn key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        match self.modal.take() {
            // Any key closes the help.
            Some(Modal::Help) => return Action::None,
            Some(Modal::Delete(name)) => {
                return match key.code {
                    KeyCode::Char('y' | 'Y') => self.job(name, Verb::Delete),
                    KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc => Action::None,
                    _ => {
                        self.modal = Some(Modal::Delete(name));
                        Action::None
                    }
                };
            }
            None => {}
        }

        let len = self.vms.as_ref().map_or(0, Vec::len);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Char('n') => return Action::Run(Foreground::New),
            KeyCode::Up | KeyCode::Char('k') => self.table.select_previous(),
            KeyCode::Down | KeyCode::Char('j') if len > 0 => {
                self.table.select(Some(self.table.selected().map_or(0, |i| (i + 1).min(len - 1))));
            }
            KeyCode::Home | KeyCode::Char('g') if len > 0 => self.table.select_first(),
            KeyCode::End | KeyCode::Char('G') if len > 0 => self.table.select(Some(len - 1)),
            KeyCode::Enter => {
                if let Some((name, _)) = self.target() {
                    return Action::Run(Foreground::Ssh(name));
                }
            }
            KeyCode::Char('l') => {
                if let Some((name, _)) = self.target() {
                    return Action::Run(Foreground::Logs(name));
                }
            }
            KeyCode::Char('c') => match self.target() {
                Some((name, State::Stopped)) => self.say(format!("{name} isn't running; press s to start it"), WARN),
                Some((name, _)) => return Action::Run(Foreground::Console(name)),
                None => {}
            },
            KeyCode::Char('s') => match self.target() {
                Some((name, State::Stopped)) => return self.job(name, Verb::Start),
                Some((name, state)) => self.say(format!("{name} is already {state}"), DIM),
                None => {}
            },
            KeyCode::Char(c @ ('x' | 'X')) => match self.target() {
                Some((name, State::Stopped)) => self.say(format!("{name} is already stopped"), DIM),
                Some((name, _)) => return self.job(name, if c == 'X' { Verb::ForceStop } else { Verb::Stop }),
                None => {}
            },
            KeyCode::Char('p') => match self.target() {
                Some((name, State::Running)) => return self.job(name, Verb::Pause),
                Some((name, State::Paused)) => return self.job(name, Verb::Resume),
                Some((name, state)) => self.say(format!("{name} is {state}; only running VMs can pause"), DIM),
                None => {}
            },
            KeyCode::Char('d') => {
                // A broken VM can't be loaded to delete it, so say why instead.
                if let Some((name, _)) = self.target() {
                    self.modal = Some(Modal::Delete(name));
                }
            }
            _ => {}
        }
        Action::None
    }

    fn draw(&mut self, frame: &mut Frame) {
        if self.status.as_ref().is_some_and(|s| s.at.elapsed() > STATUS_FOR) {
            self.status = None;
        }
        let area = frame.area();
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(BORDER)
            .title(self.title())
            .title_bottom(self.hints());
        if let Some(counts) = self.counts() {
            block = block.title(counts.right_aligned());
        }
        let inner = block.inner(area);
        frame.render_widget(block, area);
        // One column of breathing room inside the box.
        let body = inner.inner(Margin { horizontal: 1, vertical: 0 });

        match &self.vms {
            None => frame.render_widget(Paragraph::new(Line::styled("loading…", DIM)), body),
            Some(vms) if vms.is_empty() => draw_empty(frame, body),
            Some(vms) => {
                let spinner = SPINNER[self.tick % SPINNER.len()];
                let busy: HashMap<&str, Verb> = self.busy.iter().map(|(n, b)| (n.as_str(), b.verb)).collect();
                frame.render_stateful_widget(table(vms, self.table.selected(), &busy, spinner), body, &mut self.table);
                // Rows below the header; a scrollbar on the box's edge when they don't all fit.
                let visible = body.height.saturating_sub(1) as usize;
                if vms.len() > visible {
                    let mut scroll = ScrollbarState::new(vms.len().saturating_sub(visible))
                        .position(self.table.offset())
                        .viewport_content_length(visible);
                    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                        .begin_symbol(None)
                        .end_symbol(None)
                        .track_symbol(Some("│"))
                        .thumb_symbol("┃")
                        .track_style(BORDER)
                        .thumb_style(Style::new().fg(Color::Cyan));
                    let track = Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..area };
                    frame.render_stateful_widget(bar, track, &mut scroll);
                }
            }
        }

        match &self.modal {
            Some(Modal::Help) => draw_help(frame, area),
            Some(Modal::Delete(name)) => draw_delete(frame, area, name),
            None => {}
        }
    }

    /// `vx`, then the latest message, in the top border.
    fn title(&self) -> Line<'static> {
        let mut spans = vec![Span::styled("─ ", BORDER), Span::styled("vx", ACCENT), Span::raw(" ")];
        if let Some(status) = &self.status {
            spans.push(Span::styled("─ ", BORDER));
            spans.push(Span::styled(status.text.clone(), status.style));
            spans.push(Span::raw(" "));
        }
        Line::from(spans)
    }

    /// `2 VMs · 1 running`, in the top border.
    fn counts(&self) -> Option<Line<'static>> {
        if let Some(error) = &self.error {
            return Some(Line::from(vec![Span::styled(format!(" {error}"), ERROR), Span::styled(" ─", BORDER)]));
        }
        let vms = self.vms.as_ref()?;
        let running = vms.iter().filter(|e| matches!(e.vm, Ok((_, State::Running)))).count();
        let plural = if vms.len() == 1 { "" } else { "s" };
        Some(Line::from(vec![
            Span::styled(format!(" {} VM{plural} · ", vms.len()), DIM),
            Span::styled(format!("{running} running"), if running > 0 { OK } else { DIM }),
            Span::styled(" ─", BORDER),
        ]))
    }

    /// Keys for what the selected VM can do, in the bottom border.
    fn hints(&self) -> Line<'static> {
        let mut spans = vec![Span::styled("─ ", BORDER)];
        let entry = self.selected();
        let mut keys: Vec<(&str, &str)> = Vec::new();
        match entry.map(|e| (&e.name, &e.vm)) {
            Some((name, Err(e))) => {
                spans.push(Span::styled(format!("{name}: {e}"), ERROR));
                spans.push(Span::styled(" · ", BORDER));
            }
            Some((name, Ok((_, state)))) if !self.busy.contains_key(name) => {
                keys.push(("⏎", "ssh"));
                match state {
                    State::Stopped => keys.extend([("s", "start"), ("d", "delete")]),
                    State::Paused => keys.extend([("p", "resume"), ("x", "stop")]),
                    _ => keys.extend([("c", "console"), ("x", "stop"), ("p", "pause")]),
                }
            }
            _ => {}
        }
        keys.extend([("n", "new"), ("?", "keys"), ("q", "quit")]);
        for (i, (key, what)) in keys.into_iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", BORDER));
            }
            spans.push(Span::styled(key.to_string(), ACCENT));
            spans.push(Span::styled(format!(" {what}"), DIM));
        }
        spans.push(Span::raw(" "));
        Line::from(spans)
    }
}

/// Run `vx <verb> <name>` on a thread, in its own process group so Ctrl-C in an ssh
/// session can't interrupt it, and report how it went.
fn spawn_job(io: &Io, name: String, verb: Verb) {
    let (tx, exe, home) = (io.tx.clone(), io.exe.clone(), io.home.clone());
    thread::spawn(move || {
        let output = Command::new(exe)
            .args(verb.args())
            .arg(&name)
            .env("VX_HOME", home)
            .stdin(Stdio::null())
            .process_group(0)
            .output();
        let result = match output {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(error_line(&String::from_utf8_lossy(&out.stderr))),
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(Msg::Done { name, verb, result });
    });
}

/// The CLI's `error:` and `hint:` lines as one line.
fn error_line(stderr: &str) -> String {
    let field = |label: &str| stderr.lines().find_map(|l| l.strip_prefix(label)).map(str::trim);
    match (field("error:"), field("hint:")) {
        (Some(error), Some(hint)) => format!("{error} · {hint}"),
        (Some(error), None) => error.to_string(),
        _ => stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("failed").to_string(),
    }
}

/// Run an interactive command on the plain terminal, then hold the screen if it failed.
fn run_foreground(io: &Io, fg: &Foreground) -> Result<()> {
    if let Foreground::Logs(name) = fg {
        eprintln!("{}", ERR.dim(format!("{name}'s boot log · Ctrl-C returns to the dashboard")));
    }
    let status = Command::new(&io.exe).args(fg.args()).env("VX_HOME", &io.home).status()?;
    if fg.hold(status) {
        eprint!("\n{}", ERR.dim("press Enter to return to the dashboard"));
        io::stderr().flush()?;
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
    }
    Ok(())
}

/// Hand the terminal back for `run`, then take it again.
fn suspend(terminal: &mut DefaultTerminal, run: impl FnOnce() -> Result<()>) -> Result<()> {
    terminal::disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, cursor::Show)?;
    let result = run();
    execute!(io::stdout(), EnterAlternateScreen)?;
    terminal::enable_raw_mode()?;
    terminal.clear()?;
    result
}

/// The selected row is a dark bar; its text is forced to white so it reads on light themes too.
fn table(vms: &[Entry], selected: Option<usize>, busy: &HashMap<&str, Verb>, spinner: char) -> Table<'static> {
    let right = |s: String| Cell::from(Line::from(s).alignment(Alignment::Right));
    let rows = vms.iter().enumerate().map(|(i, entry)| {
        let text = if selected == Some(i) { SELECTED_TEXT } else { Style::new() };
        let quiet = if selected == Some(i) { SELECTED_TEXT } else { DIM };
        let row = match &entry.vm {
            Ok((spec, state)) => {
                let state = match busy.get(entry.name.as_str()) {
                    Some(verb) => Line::styled(format!("{spinner} {}", verb.doing()), BUSY),
                    None => {
                        let (dot, color) = match state {
                            State::Running => ("●", OK),
                            State::Stopped => ("○", DIM),
                            _ => ("◐", WARN),
                        };
                        Line::from(vec![Span::styled(dot, color), Span::styled(format!(" {state}"), color)])
                    }
                };
                vec![
                    Cell::from(clip(&entry.name, NAME_MAX)),
                    Cell::from(state),
                    Cell::from(clip(&spec.image, IMAGE_MAX)),
                    Cell::from(spec.arch.to_string()),
                    right(spec.cpus.to_string()),
                    right(spec.memory.clone()),
                    Cell::from(Span::styled(format!("127.0.0.1:{}", spec.ssh.port), quiet)),
                ]
            }
            // The full error shows in the bottom border when the row is selected.
            Err(_) => vec![Cell::from(clip(&entry.name, NAME_MAX)), Cell::from(Span::styled("✗ broken", ERROR))],
        };
        Row::new(row).style(text)
    });
    let width = |f: fn(&Entry) -> usize, min: usize, max: usize| vms.iter().map(f).max().unwrap_or(0).clamp(min, max) as u16;
    let widths = [
        Constraint::Length(width(|e| e.name.chars().count(), 4, NAME_MAX)),
        Constraint::Length(10),
        Constraint::Length(width(|e| e.vm.as_ref().map_or(0, |(s, _)| s.image.chars().count()), 5, IMAGE_MAX)),
        Constraint::Length(7),
        Constraint::Length(4),
        Constraint::Length(6),
        Constraint::Fill(1),
    ];
    let header = Row::new(vec![
        Cell::from("NAME"),
        Cell::from("STATE"),
        Cell::from("IMAGE"),
        Cell::from("ARCH"),
        right("CPUS".into()),
        right("MEMORY".into()),
        Cell::from("SSH"),
    ])
    .style(HEADER);
    Table::new(rows, widths).header(header).column_spacing(2).row_highlight_style(SELECTED)
}

/// Columns wider than this end in `…`, so one long name can't push the rest off screen.
const NAME_MAX: usize = 24;
const IMAGE_MAX: usize = 18;

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max - 1).collect();
    clipped.push('…');
    clipped
}

fn draw_empty(frame: &mut Frame, area: Rect) {
    let [_, middle, _] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(2), Constraint::Fill(2)]).areas(area);
    let text = vec![
        Line::from("No VMs yet").alignment(Alignment::Center),
        Line::from(vec![Span::styled("press ", DIM), Span::styled("n", ACCENT), Span::styled(" to create one", DIM)])
            .alignment(Alignment::Center),
    ];
    frame.render_widget(Paragraph::new(text), middle);
}

/// A rounded box of `width` × `height` in the middle of `area`, cleared, with `title`.
fn dialog(frame: &mut Frame, area: Rect, width: u16, height: u16, title: Line<'static>, border: Style) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(height)]).flex(Flex::Center).areas(area);
    let [rect] = Layout::horizontal([Constraint::Length(width)]).flex(Flex::Center).areas(row);
    frame.render_widget(Clear, rect);
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(border).title(title);
    let inner = block.inner(rect).inner(Margin { horizontal: 2, vertical: 1 });
    frame.render_widget(block, rect);
    inner
}

const HELP: &[(&str, &str)] = &[
    ("⏎", "ssh in, starting it first if needed"),
    ("c", "serial console · Ctrl-] returns"),
    ("l", "boot log · Ctrl-C returns"),
    ("s", "start"),
    ("x / X", "stop / force it off"),
    ("p", "pause or resume"),
    ("n", "new VM"),
    ("d", "delete"),
    ("↑↓ j k", "select · g G first / last"),
    ("q", "quit"),
];

fn draw_help(frame: &mut Frame, area: Rect) {
    let title = Line::from(vec![Span::styled("─ ", BORDER), Span::styled("keys", ACCENT), Span::raw(" ")]);
    let inner = dialog(frame, area, 48, HELP.len() as u16 + 4, title, BORDER);
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(key, what)| Line::from(vec![Span::styled(format!("{key:8}"), ACCENT), Span::raw(*what)]))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_delete(frame: &mut Frame, area: Rect, name: &str) {
    let title = Line::from(vec![Span::styled("─ ", ERROR), Span::styled("delete", ERROR), Span::raw(" ")]);
    let inner = dialog(frame, area, 46, 7, title, ERROR);
    let lines = vec![
        Line::from(vec![Span::raw("Delete "), Span::styled(name.to_string(), HEADER), Span::raw(" and its disk?")]),
        Line::styled("Cached images are kept.", DIM),
        Line::default(),
        Line::from(vec![
            Span::styled("y", ACCENT),
            Span::styled(" delete · ", DIM),
            Span::styled("n", ACCENT),
            Span::styled(" cancel", DIM),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Arch;
    use crate::vx::SshSpec;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyEventState;

    fn entry(name: &str, state: State, image: &str, port: u16) -> Entry {
        let spec = Spec {
            backend: "qemu".into(),
            image: image.into(),
            arch: Arch::Aarch64,
            cpus: 4,
            memory: "4G".into(),
            forward: vec![],
            ssh: SshSpec { user: "me".into(), port },
            qemu: None,
        };
        Entry { name: name.into(), vm: Ok((spec, state)) }
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE }
    }

    fn vms() -> Vec<Entry> {
        vec![
            entry("dev", State::Running, "debian-13", 2222),
            entry("web", State::Stopped, "fedora-44", 2223),
            Entry { name: "old".into(), vm: Err("invalid vx.toml".into()) },
        ]
    }

    fn with_vms() -> App {
        let mut app = App::default();
        app.update(vms());
        app
    }

    #[test]
    fn draws_the_table() {
        let mut app = with_vms();
        assert_eq!(
            screen(&mut app, 72, 7),
            "╭─ vx ───────────────────────────────────────────── 3 VMs · 1 running ─╮
│ NAME  STATE       IMAGE      ARCH     CPUS  MEMORY  SSH              │
│ dev   ● running   debian-13  aarch64     4      4G  127.0.0.1:2222   │
│ web   ○ stopped   fedora-44  aarch64     4      4G  127.0.0.1:2223   │
│ old   ✗ broken                                                       │
│                                                                      │
╰─ ⏎ ssh · c console · x stop · p pause · n new · ? keys · q quit ─────╯"
        );
        let _ = app.key(press(KeyCode::End));
        assert!(screen(&mut app, 72, 7).contains("╰─ old: invalid vx.toml · n new · ? keys · q quit ─"));
    }

    #[test]
    fn hints_follow_the_selected_state() {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Down));
        assert!(screen(&mut app, 80, 7).contains("⏎ ssh · s start · d delete · n new"));
    }

    #[test]
    fn shows_loading_then_empty() {
        let mut app = App::default();
        assert!(screen(&mut app, 60, 6).contains("loading…"));
        app.update(vec![]);
        let text = screen(&mut app, 60, 8);
        assert!(text.contains("No VMs yet") && text.contains("press n to create one"), "{text}");
        assert!(text.lines().next().unwrap().ends_with(" 0 VMs · 0 running ─╮"), "{text}");
    }

    #[test]
    fn selection_moves_and_survives_refreshes() {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Down));
        assert_eq!(app.selected().unwrap().name, "web");
        let _ = app.key(press(KeyCode::Down));
        let _ = app.key(press(KeyCode::Down)); // stops at the end
        assert_eq!(app.selected().unwrap().name, "old");
        let _ = app.key(press(KeyCode::Char('k')));

        // `web` stays selected when a VM appears before it…
        let mut more = vms();
        more.insert(0, entry("api", State::Running, "debian-13", 2224));
        app.update(more);
        assert_eq!(app.selected().unwrap().name, "web");
        // …and the selection stays in range when VMs disappear.
        app.update(vec![entry("dev", State::Running, "debian-13", 2222)]);
        assert_eq!(app.selected().unwrap().name, "dev");
        app.update(vec![]);
        assert!(app.selected().is_none());
    }

    #[test]
    fn actions_fit_the_state() {
        let mut app = with_vms(); // `dev` is running
        assert_eq!(app.key(press(KeyCode::Enter)), Action::Run(Foreground::Ssh("dev".into())));
        assert_eq!(app.key(press(KeyCode::Char('c'))), Action::Run(Foreground::Console("dev".into())));
        assert_eq!(app.key(press(KeyCode::Char('l'))), Action::Run(Foreground::Logs("dev".into())));
        assert_eq!(app.key(press(KeyCode::Char('n'))), Action::Run(Foreground::New));
        assert_eq!(app.key(press(KeyCode::Char('s'))), Action::None);
        assert_eq!(app.status.as_ref().unwrap().text, "dev is already running");
        assert_eq!(app.key(press(KeyCode::Char('p'))), Action::Job("dev".into(), Verb::Pause));

        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Down)); // `web` is stopped
        assert_eq!(app.key(press(KeyCode::Char('c'))), Action::None);
        assert!(app.status.as_ref().unwrap().text.contains("press s to start it"));
        assert_eq!(app.key(press(KeyCode::Char('x'))), Action::None);
        assert_eq!(app.key(press(KeyCode::Char('s'))), Action::Job("web".into(), Verb::Start));
    }

    #[test]
    fn busy_vms_show_a_spinner_and_refuse_more_work() {
        let mut app = with_vms();
        assert_eq!(app.key(press(KeyCode::Char('X'))), Action::Job("dev".into(), Verb::ForceStop));
        assert!(screen(&mut app, 72, 7).contains(" stopping "));
        assert_eq!(app.key(press(KeyCode::Char('x'))), Action::None);
        assert_eq!(app.status.as_ref().unwrap().text, "dev is busy stopping");

        // Done: the message shows at once, the spinner stays until a refresh brings the new state.
        app.busy.get_mut("dev").unwrap().done = true;
        app.say("✓ dev stopped".into(), OK);
        assert!(screen(&mut app, 72, 7).starts_with("╭─ vx ─ ✓ dev stopped ─"));
        app.update(vec![entry("dev", State::Stopped, "debian-13", 2222)]);
        assert!(app.busy.is_empty());
        assert!(screen(&mut app, 72, 7).contains("○ stopped"));
    }

    #[test]
    fn delete_asks_first() {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Char('d')));
        assert_eq!(app.modal, Some(Modal::Delete("dev".into())));
        let text = screen(&mut app, 72, 12);
        assert!(text.contains("Delete dev and its disk?"), "{text}");
        assert_eq!(app.key(press(KeyCode::Char('z'))), Action::None); // ignored, still asking
        assert_eq!(app.key(press(KeyCode::Char('n'))), Action::None);
        assert!(app.modal.is_none());
        let _ = app.key(press(KeyCode::Char('d')));
        assert_eq!(app.key(press(KeyCode::Char('y'))), Action::Job("dev".into(), Verb::Delete));
    }

    #[test]
    fn broken_vms_say_why_instead_of_acting() {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::End));
        assert_eq!(app.key(press(KeyCode::Char('d'))), Action::None);
        assert!(app.modal.is_none());
        assert_eq!(app.status.as_ref().unwrap().text, "✗ old: invalid vx.toml");
    }

    #[test]
    fn help_opens_and_any_key_closes_it() {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Char('?')));
        let text = screen(&mut app, 72, 20);
        assert!(text.contains("─ keys ─") && text.contains("pause or resume"), "{text}");
        assert_eq!(app.key(press(KeyCode::Char('q'))), Action::None); // closes help, doesn't quit
        assert!(app.modal.is_none());
        assert_eq!(app.key(press(KeyCode::Char('q'))), Action::Quit);
    }

    #[test]
    fn errors_from_the_cli_become_one_line() {
        let stderr = "error: QEMU could not start web: port in use\nhint: change [ssh] port in vx.toml\n";
        assert_eq!(error_line(stderr), "QEMU could not start web: port in use · change [ssh] port in vx.toml");
        assert_eq!(error_line("error: web is already running\n"), "web is already running");
        assert_eq!(error_line("something odd\n\n"), "something odd");
    }

    #[test]
    fn scrollbar_only_when_rows_overflow() {
        let many: Vec<Entry> = (0..12).map(|i| entry(&format!("vm-{i:02}"), State::Stopped, "debian-13", 2222 + i)).collect();
        let mut app = App::default();
        app.update(many[..3].to_vec());
        assert!(!screen(&mut app, 72, 9).contains('┃'));
        app.update(many);
        for _ in 0..11 {
            let _ = app.key(press(KeyCode::Down));
        }
        let text = screen(&mut app, 72, 9);
        assert!(text.contains("vm-11"), "the selection scrolls into view:\n{text}");
        assert!(text.contains('┃'), "{text}");
    }

    #[test]
    fn long_names_are_clipped() {
        assert_eq!(clip("debian-13", IMAGE_MAX), "debian-13");
        assert_eq!(clip("debian-13-aarch64-d8470b8c6c38", IMAGE_MAX), "debian-13-aarch64…");
        let mut app = App::default();
        app.update(vec![entry("dev", State::Running, "debian-13-aarch64-d8470b8c6c38", 2222)]);
        let text = screen(&mut app, 80, 6);
        assert!(text.contains("debian-13-aarch64…  aarch64") && text.contains("127.0.0.1:2222"), "{text}");
    }
}
