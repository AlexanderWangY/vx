//! The dashboard: `vx` with no arguments. Two views in one box, switched with Tab: a table of VMs
//! whose state refreshes every second, and a table of images.
//!
//! On a wide enough terminal, the selected VM's details sit to the right of the table, with live
//! CPU, memory, disk and network numbers from inside it while it runs (see `stats`). `S` swaps
//! the table for the VM's snapshot history (see `snapshot`) until Esc.
//!
//! Actions run the CLI itself as a child process, so they share its locks, checks and messages:
//! most in the background with their output captured, and the interactive ones (ssh, console,
//! logs) in the foreground with the dashboard set aside until they end. Creating a VM and adding
//! an image use forms drawn in the dashboard.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
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
    Wrap,
};
use ratatui::{DefaultTerminal, Frame};

use crate::NewArgs;
use crate::backend::{self, State};
use crate::form::{AddImageForm, NewForm, NewImage, NewSnap, PortChange, PortsForm, SnapForm, Step};
use crate::host::Arch;
use crate::image::{self, Info};
use crate::progress::bytes;
use crate::setup;
use crate::snapshot::{self, History};
use crate::stats::{Sample, Stats, Watcher};
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
/// Snapshots: one with memory, which resumes running; one of the disk alone, which boots.
const LIVE: Style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
const DISK: Style = Style::new().fg(Color::LightBlue);
/// The current snapshot: a solid label, so it shows even on the selected row.
const HERE: Style = Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD);
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
    /// Bytes its disk takes up on the host, if the backend knows.
    pub disk: Option<u64>,
    /// Packages or the setup script are going in right now.
    pub setting_up: bool,
}

/// Every VM, sorted by name, with its state asked of its backend.
pub fn entries(home: &Home) -> Result<Vec<Entry>> {
    Ok(home
        .names()?
        .into_iter()
        .map(|name| {
            let mut disk = None;
            let vm = home.load(&name).map_err(|e| format!("{e:#}")).map(|vm| {
                let state = match backend::get(&vm.spec.backend) {
                    Ok(b) => {
                        disk = b.disk_usage(&vm);
                        b.state(&vm)
                    }
                    Err(_) => State::Other(format!("unknown backend `{}`", vm.spec.backend)),
                };
                (vm.spec, state)
            });
            let setting_up = home.vms().join(&name).join(setup::MARKER).exists();
            Entry { name, vm, disk, setting_up }
        })
        .collect())
}

/// What the other threads tell the dashboard.
enum Msg {
    Snapshot {
        vms: Result<Vec<Entry>, String>,
        images: Vec<Info>,
        /// The focused VM's snapshot history, by VM name.
        history: Option<(String, Result<History, String>)>,
    },
    /// A new reading from inside a running VM.
    Stats { name: String, sample: Sample },
    /// A background job finished.
    Done { target: Target, verb: Verb, result: Result<(), String>, done: Option<String> },
}

/// Everything the main loop needs besides the app state.
struct Io {
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
    /// Asks the refresher for a refresh now rather than at the next tick.
    wake: Sender<()>,
    /// The VM whose snapshot history the refresher loads too.
    focus: Arc<Mutex<Option<String>>>,
    exe: PathBuf,
    home: PathBuf,
}

pub fn run(home: &Home) -> Result<()> {
    let host = Arch::host()?;
    ignore_sigint();
    let (tx, rx) = mpsc::channel();
    let focus = Arc::new(Mutex::new(None));
    let wake = refresher(home, host, tx.clone(), focus.clone());
    let io = Io { rx, tx, wake, focus, exe: env::current_exe()?, home: home.root().to_path_buf() };
    ratatui::run(|terminal| App::new(home, host).run(terminal, &io))
}

/// Ctrl-C in an ssh session or `vx logs -f` should end that, not the dashboard. A no-op handler
/// (rather than SIG_IGN, which children inherit) keeps us alive while children get the default.
fn ignore_sigint() {
    extern "C" fn ignore(_: libc::c_int) {}
    // SAFETY: the handler does nothing, so it's async-signal-safe.
    unsafe { libc::signal(libc::SIGINT, ignore as extern "C" fn(libc::c_int) as libc::sighandler_t) };
}

/// Reload VMs and images, and the focused VM's snapshots, on a thread, so a slow VM never
/// freezes the screen.
fn refresher(home: &Home, host: Arch, tx: Sender<Msg>, focus: Arc<Mutex<Option<String>>>) -> Sender<()> {
    let (wake, woken) = mpsc::channel();
    let home = Home::at(home.root());
    thread::spawn(move || {
        loop {
            let vms = entries(&home).map_err(|e| format!("{e:#}"));
            let focused = focus.lock().unwrap().clone();
            let history = focused.map(|name| {
                let history = history(&home, &name).map_err(|e| format!("{e:#}"));
                (name, history)
            });
            if tx.send(Msg::Snapshot { vms, images: image::infos(&home, host), history }).is_err() {
                return; // the dashboard closed
            }
            if let Err(RecvTimeoutError::Disconnected) = woken.recv_timeout(REFRESH) {
                return;
            }
        }
    });
    wake
}

fn history(home: &Home, name: &str) -> Result<History> {
    let vm = home.load(name)?;
    match backend::get(&vm.spec.backend)?.snapshots() {
        Some(snapshots) => History::load(&vm, snapshots),
        None => anyhow::bail!("the {} backend can't take snapshots", vm.spec.backend),
    }
}

/// What a background job is about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Target {
    Vm(String),
    Image(String),
}

impl Target {
    fn name(&self) -> &str {
        match self {
            Target::Vm(name) | Target::Image(name) => name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Start,
    Stop,
    ForceStop,
    Pause,
    Resume,
    Delete,
    Create,
    Pull,
    RemoveImage,
    AddImage,
    Snapshot,
    Restore,
    DeleteSnapshot,
    Forward,
    Unforward,
}

impl Verb {
    /// The `vx` arguments for a VM job; the name goes last.
    fn vm_args(self) -> &'static [&'static str] {
        match self {
            Verb::Start => &["start"],
            Verb::Stop => &["stop"],
            Verb::ForceStop => &["stop", "--force"],
            Verb::Pause => &["pause"],
            Verb::Resume => &["resume"],
            Verb::Delete => &["rm", "-y"],
            _ => &[],
        }
    }

    /// Shown in place of the state while it runs.
    fn doing(self) -> &'static str {
        match self {
            Verb::Start => "starting",
            Verb::Stop | Verb::ForceStop => "stopping",
            Verb::Pause => "pausing",
            Verb::Resume => "resuming",
            Verb::Delete | Verb::RemoveImage => "deleting",
            Verb::Create => "creating",
            Verb::Pull => "downloading",
            Verb::AddImage => "copying",
            Verb::Snapshot => "saving",
            Verb::Restore => "restoring",
            Verb::DeleteSnapshot => "deleting",
            Verb::Forward => "forwarding",
            Verb::Unforward => "unforwarding",
        }
    }

    fn done(self) -> &'static str {
        match self {
            Verb::Start => "started",
            Verb::Stop | Verb::ForceStop => "stopped",
            Verb::Pause => "paused",
            Verb::Resume => "resumed",
            Verb::Delete | Verb::RemoveImage => "deleted",
            Verb::Create => "is ready",
            Verb::Pull => "downloaded",
            Verb::AddImage => "added",
            Verb::Snapshot => "saved",
            Verb::Restore => "restored",
            Verb::DeleteSnapshot => "deleted",
            Verb::Forward => "forwarded",
            Verb::Unforward => "unforwarded",
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
            Verb::Delete | Verb::RemoveImage => "delete",
            Verb::Create => "create",
            Verb::Pull => "download",
            Verb::AddImage => "add",
            Verb::Snapshot => "save",
            Verb::Restore => "restore",
            Verb::DeleteSnapshot => "delete",
            Verb::Forward => "forward a port to",
            Verb::Unforward => "stop forwarding a port to",
        })
    }
}

/// A background `vx` run.
#[derive(Debug, PartialEq, Eq)]
struct Job {
    target: Target,
    verb: Verb,
    args: Vec<String>,
    /// Run after `args` succeeds.
    then: Option<Vec<String>>,
    /// Says what was done when it succeeds, in place of `<target> <verb.done()>`.
    done: Option<String>,
}

/// Interactive commands, which take over the terminal until they end.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Foreground {
    Ssh(String),
    Console(String),
    Logs(String),
}

impl Foreground {
    fn args(&self) -> Vec<String> {
        let args: &[&str] = match self {
            Foreground::Ssh(name) => &["ssh", name],
            Foreground::Console(name) => &["console", name],
            Foreground::Logs(name) => &["logs", "-f", name],
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
    Job(Job),
    Run(Foreground),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum View {
    #[default]
    Vms,
    Images,
    /// The snapshots of `App::snap_vm`.
    Snapshots,
}

enum Modal {
    Help,
    /// Asking before deleting this VM.
    DeleteVm(String),
    /// Asking before deleting this image's local copy.
    DeleteImage {
        name: String,
        size: u64,
        custom: bool,
    },
    New(Box<NewForm>),
    AddImage(Box<AddImageForm>),
    Snap(Box<SnapForm>),
    /// The ports of this VM.
    Ports(String, Box<PortsForm>),
    /// Asking before restoring the snapshot view's VM to this snapshot.
    Restore(String),
    /// Asking before deleting this snapshot.
    DeleteSnapshot(String),
}

/// A VM being created, shown as a row before its directory exists.
struct Pending {
    image: String,
    arch: Arch,
    cpus: String,
    memory: String,
}

struct Busy {
    verb: Verb,
    /// Finished; cleared once a refresh shows the outcome, so the old state never flashes back.
    done: bool,
    pending: Option<Pending>,
}

struct Status {
    text: String,
    style: Style,
    at: Instant,
}

struct App {
    home: Home,
    host: Arch,
    view: View,
    /// `None` until the first refresh arrives.
    vms: Option<Vec<Entry>>,
    images: Vec<Info>,
    /// Why the last refresh failed, e.g. VX_HOME isn't readable.
    error: Option<String>,
    table: TableState,
    image_table: TableState,
    busy: HashMap<Target, Busy>,
    status: Option<Status>,
    modal: Option<Modal>,
    /// Select this VM as soon as it shows up, e.g. one just created.
    follow: Option<String>,
    /// Advances every tick, for the spinners.
    tick: usize,
    /// Live numbers from inside running VMs, by name.
    stats: HashMap<String, Stats>,
    /// Whether to show the details pane when there's room; `i` toggles it.
    details: bool,
    /// Whether the last frame had room for it.
    pane: bool,
    /// The focused VM's snapshots: the snapshot view's, or the selected VM's for the details.
    history: Option<(String, Result<History, String>)>,
    /// The VM in the snapshot view.
    snap_vm: String,
    /// The snapshot selected there; `None` until one is picked, which means the one the VM is
    /// at (see `snap_pick`).
    snap_selected: Option<String>,
    snap_table: TableState,
}

impl App {
    fn new(home: &Home, host: Arch) -> App {
        App {
            home: Home::at(home.root()),
            host,
            view: View::Vms,
            vms: None,
            images: Vec::new(),
            error: None,
            table: TableState::default(),
            image_table: TableState::default(),
            busy: HashMap::new(),
            status: None,
            modal: None,
            follow: None,
            tick: 0,
            stats: HashMap::new(),
            details: true,
            pane: false,
            history: None,
            snap_vm: String::new(),
            snap_selected: None,
            snap_table: TableState::default(),
        }
    }

    fn run(mut self, terminal: &mut DefaultTerminal, io: &Io) -> Result<()> {
        // Samples the VM whose details are showing; dropping it ends its ssh session.
        let mut watcher: Option<Watcher> = None;
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
            let focus = self.focus();
            let mut focused = io.focus.lock().unwrap();
            if *focused != focus {
                *focused = focus;
                let _ = io.wake.send(());
            }
            drop(focused);
            let watch = self.watch();
            if watcher.as_ref().map(Watcher::name) != watch {
                watcher = watch.map(|name| {
                    let (tx, name) = (io.tx.clone(), name.to_string());
                    Watcher::start(&io.home, name.clone(), move |sample| {
                        tx.send(Msg::Stats { name: name.clone(), sample }).is_ok()
                    })
                });
            }
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
                Action::Job(job) => spawn_job(io, job),
                Action::Run(fg) => {
                    suspend(terminal, || run_foreground(io, &fg))?;
                    let _ = io.wake.send(());
                }
            }
        }
    }

    fn handle(&mut self, msg: Msg, io: &Io) {
        match msg {
            Msg::Snapshot { vms: Ok(vms), images, history } => {
                self.update(vms);
                self.update_images(images);
                self.update_history(history);
            }
            Msg::Snapshot { vms: Err(e), images, history } => {
                self.error = Some(e);
                self.update_images(images);
                self.update_history(history);
            }
            Msg::Stats { name, sample } => self.stats.entry(name).or_default().push(sample),
            Msg::Done { target, verb, result, done } => {
                let name = target.name();
                match result {
                    Ok(()) => self.say(done.unwrap_or_else(|| format!("✓ {name} {}", verb.done())), OK),
                    Err(e) => self.say(format!("✗ couldn't {verb} {name}: {e}"), ERROR),
                }
                if let Some(busy) = self.busy.get_mut(&target) {
                    busy.done = true;
                }
                let _ = io.wake.send(());
            }
        }
    }

    /// Take a fresh list, keeping the same VM selected even if others came or went.
    fn update(&mut self, vms: Vec<Entry>) {
        let wanted = self.follow.clone().filter(|name| vms.iter().any(|e| &e.name == name));
        if wanted.is_some() {
            self.follow = None;
        }
        let wanted = wanted.or_else(|| self.selected().map(|e| e.name.clone()));
        let index = wanted.and_then(|name| vms.iter().position(|e| e.name == name));
        self.table.select(keep_in_range(index, self.table.selected(), vms.len()));
        self.busy.retain(|_, busy| !busy.done);
        // Numbers from a VM that stopped are stale, and its next boot starts over.
        self.stats.retain(|name, _| vms.iter().any(|e| &e.name == name && matches!(e.vm, Ok((_, State::Running)))));
        self.vms = Some(vms);
        self.error = None;
    }

    fn update_history(&mut self, history: Option<(String, Result<History, String>)>) {
        // Mid-job, the disk can be briefly unreadable; keep what was there.
        if let Some((name, Err(_))) = &history
            && self.busy.contains_key(&Target::Vm(name.clone()))
            && self.history.as_ref().is_some_and(|(n, _)| n == name)
        {
            return;
        }
        self.history = history;
        // A snapshot that's gone can't stay selected.
        if let Some(h) = self.snap_history()
            && let Some(name) = &self.snap_selected
            && h.get(name).is_none()
        {
            self.snap_selected = None;
        }
    }

    /// The VM whose snapshots to load: the snapshot view's, or the one in the details pane.
    fn focus(&self) -> Option<String> {
        match self.view {
            View::Snapshots => Some(self.snap_vm.clone()),
            View::Vms if self.pane => self.selected().filter(|e| e.vm.is_ok()).map(|e| e.name.clone()),
            _ => None,
        }
    }

    /// The history of `name`, if it's loaded.
    fn history_of(&self, name: &str) -> Option<&History> {
        match &self.history {
            Some((n, Ok(h))) if n == name => Some(h),
            _ => None,
        }
    }

    fn snap_history(&self) -> Option<&History> {
        self.history_of(&self.snap_vm)
    }

    /// The selected snapshot: the one picked, else the current one, else the newest.
    fn snap_pick(&self) -> Option<&snapshot::Entry> {
        let h = self.snap_history()?;
        let picked = self.snap_selected.as_deref().and_then(|name| h.get(name));
        picked.or_else(|| h.current.as_deref().and_then(|name| h.get(name))).or_else(|| h.entries.last())
    }

    /// The selected snapshot's place in the list.
    fn snap_index(&self) -> Option<usize> {
        let (h, pick) = (self.snap_history()?, self.snap_pick()?);
        h.entries.iter().position(|e| e.snap.name == pick.snap.name)
    }

    fn open_snapshots(&mut self, name: String) {
        if self.snap_vm != name {
            self.snap_table = TableState::default();
        }
        self.snap_selected = None;
        self.snap_vm = name;
        self.view = View::Snapshots;
    }

    fn update_images(&mut self, images: Vec<Info>) {
        let wanted = self.selected_image().map(|i| i.name.clone());
        let index = wanted.and_then(|name| images.iter().position(|i| i.name == name));
        self.image_table.select(keep_in_range(index, self.image_table.selected(), images.len()));
        self.images = images;
    }

    fn selected(&self) -> Option<&Entry> {
        self.vms.as_ref()?.get(self.table.selected()?)
    }

    /// The VM to sample: the one in the details pane, while it runs and nothing else is
    /// happening to it.
    fn watch(&self) -> Option<&str> {
        if !self.pane || self.view != View::Vms {
            return None;
        }
        let entry = self.selected()?;
        let running = matches!(entry.vm, Ok((_, State::Running)));
        (running && !self.busy.contains_key(&Target::Vm(entry.name.clone()))).then_some(entry.name.as_str())
    }

    fn selected_image(&self) -> Option<&Info> {
        self.images.get(self.image_table.selected()?)
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
            Ok(_) => match self.busy.get(&Target::Vm(name.clone())) {
                Some(busy) => Err(format!("{name} is busy {}", busy.verb.doing())),
                None => Ok(entry.vm.as_ref().map(|(_, state)| state.clone()).unwrap_or(State::Stopped)),
            },
        };
        match found {
            Ok(state) => Some((name, state)),
            Err(why) => {
                self.say(why, WARN);
                None
            }
        }
    }

    fn job(&mut self, target: Target, verb: Verb, args: Vec<String>) -> Action {
        self.busy.insert(target.clone(), Busy { verb, done: false, pending: None });
        Action::Job(Job { target, verb, args, then: None, done: None })
    }

    fn vm_job(&mut self, name: String, verb: Verb) -> Action {
        let mut args: Vec<String> = verb.vm_args().iter().map(|s| s.to_string()).collect();
        args.push(name.clone());
        self.job(Target::Vm(name), verb, args)
    }

    /// Start `vx new` in the background; the VM shows as a row right away.
    fn create(&mut self, args: NewArgs) -> Action {
        let name = args.name.clone().unwrap_or_default();
        let pending = Pending {
            image: args.image.clone(),
            arch: args.arch.unwrap_or(self.host),
            cpus: args.cpus.map(|c| c.to_string()).unwrap_or_default(),
            memory: args.mem.clone().unwrap_or_default(),
        };
        self.busy.insert(Target::Vm(name.clone()), Busy { verb: Verb::Create, done: false, pending: Some(pending) });
        self.view = View::Vms;
        self.follow = Some(name.clone());
        Action::Job(Job { target: Target::Vm(name), verb: Verb::Create, args: new_argv(&args), then: None, done: None })
    }

    fn open_new(&mut self, image: Option<String>) {
        let args = NewArgs {
            name: None,
            interactive: true,
            image: image.unwrap_or_else(|| image::DEFAULT.into()),
            cpus: None,
            mem: None,
            disk: "20G".into(),
            arch: Some(self.host),
            no_start: false,
            install: vec![],
            setup: None,
            bare: false,
            mount: vec![],
        };
        match NewForm::new(&self.home, args) {
            Ok(form) => self.modal = Some(Modal::New(Box::new(form))),
            Err(e) => self.say(format!("✗ {e:#}"), ERROR),
        }
    }

    fn key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        if let Some(modal) = self.modal.take() {
            return self.modal_key(modal, key);
        }
        match key.code {
            KeyCode::Esc if self.view == View::Snapshots => self.view = View::Vms,
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Tab | KeyCode::BackTab => {
                self.view = match self.view {
                    View::Vms => View::Images,
                    View::Images | View::Snapshots => View::Vms,
                }
            }
            _ => {
                return match self.view {
                    View::Vms => self.vm_key(key),
                    View::Images => self.image_key(key),
                    View::Snapshots => self.snap_key(key),
                };
            }
        }
        Action::None
    }

    fn modal_key(&mut self, modal: Modal, key: KeyEvent) -> Action {
        let confirm = |key: KeyEvent| match key.code {
            KeyCode::Char('y' | 'Y') => Some(true),
            KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc => Some(false),
            _ => None,
        };
        match modal {
            // Any key closes the help.
            Modal::Help => {}
            Modal::DeleteVm(name) => match confirm(key) {
                Some(true) => return self.vm_job(name, Verb::Delete),
                Some(false) => {}
                None => self.modal = Some(Modal::DeleteVm(name)),
            },
            Modal::DeleteImage { name, size, custom } => match confirm(key) {
                Some(true) => {
                    let args = vec!["images".into(), "rm".into(), name.clone()];
                    return self.job(Target::Image(name), Verb::RemoveImage, args);
                }
                Some(false) => {}
                None => self.modal = Some(Modal::DeleteImage { name, size, custom }),
            },
            Modal::New(mut form) => match form.handle(key) {
                Step::Done(args) => return self.create(args),
                Step::Cancel => {}
                Step::Continue => self.modal = Some(Modal::New(form)),
            },
            Modal::AddImage(mut form) => match form.handle(key) {
                Step::Done(NewImage { name, file }) => {
                    let args = vec!["images".into(), "add".into(), name.clone(), file];
                    return self.job(Target::Image(name), Verb::AddImage, args);
                }
                Step::Cancel => {}
                Step::Continue => self.modal = Some(Modal::AddImage(form)),
            },
            Modal::Snap(mut form) => match form.handle(key) {
                Step::Done(NewSnap { name, note }) => {
                    let vm = self.snap_vm.clone();
                    let mut args = vec!["snap".to_string(), vm.clone(), name.clone()];
                    if !note.is_empty() {
                        args.extend(["-m".into(), note]);
                    }
                    self.snap_selected = None;
                    return self.vm_task(vm.clone(), Verb::Snapshot, args, None, format!("✓ saved {vm} as {name}"));
                }
                Step::Cancel => {}
                Step::Continue => self.modal = Some(Modal::Snap(form)),
            },
            Modal::Ports(vm, mut form) => match form.handle(key) {
                Step::Done(PortChange::Add(host, guest)) => {
                    let args = vec!["port".into(), vm.clone(), format!("{host}:{guest}")];
                    return self.vm_task(
                        vm.clone(),
                        Verb::Forward,
                        args,
                        None,
                        format!("✓ localhost:{host} → {vm}:{guest}"),
                    );
                }
                Step::Done(PortChange::Remove(host)) => {
                    let args = vec!["port".into(), "rm".into(), vm.clone(), host.to_string()];
                    return self.vm_task(
                        vm,
                        Verb::Unforward,
                        args,
                        None,
                        format!("✓ stopped forwarding localhost:{host}"),
                    );
                }
                Step::Cancel => {}
                Step::Continue => self.modal = Some(Modal::Ports(vm, form)),
            },
            Modal::Restore(name) => {
                let vm = self.snap_vm.clone();
                let restore = vec!["snap".into(), "restore".into(), "-y".into(), vm.clone(), name.clone()];
                let done = format!("✓ {vm} is back at {name}");
                match key.code {
                    KeyCode::Char('y' | 'Y') => {
                        self.snap_selected = None;
                        return self.vm_task(vm, Verb::Restore, restore, None, done);
                    }
                    // Snapshot what it has now, then go back.
                    KeyCode::Char('s' | 'S') => {
                        let save = vec!["snap".to_string(), vm.clone()];
                        self.snap_selected = None;
                        return self.vm_task(vm, Verb::Restore, save, Some(restore), done);
                    }
                    KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc => {}
                    _ => self.modal = Some(Modal::Restore(name)),
                }
            }
            Modal::DeleteSnapshot(name) => match confirm(key) {
                Some(true) => {
                    let vm = self.snap_vm.clone();
                    let args = vec!["snap".into(), "rm".into(), "-y".into(), vm.clone(), name.clone()];
                    let done = format!("✓ deleted {name} from {vm}");
                    return self.vm_task(vm, Verb::DeleteSnapshot, args, None, done);
                }
                Some(false) => {}
                None => self.modal = Some(Modal::DeleteSnapshot(name)),
            },
        }
        Action::None
    }

    fn vm_key(&mut self, key: KeyEvent) -> Action {
        let len = self.vms.as_ref().map_or(0, Vec::len);
        if navigate(&mut self.table, key.code, len) {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('n') => self.open_new(None),
            KeyCode::Char('f') => {
                if let Some((name, _)) = self.target()
                    && let Some(Entry { vm: Ok((spec, _)), .. }) = self.selected()
                {
                    let form = PortsForm::new(&name, spec.ssh.port, spec.forwards().collect());
                    self.modal = Some(Modal::Ports(name, Box::new(form)));
                }
            }
            KeyCode::Char('S') => {
                if let Some((name, _)) = self.target() {
                    self.open_snapshots(name);
                }
            }
            KeyCode::Char('s') if ctrl => {
                if let Some((name, _)) = self.target() {
                    return self.quick_snap(name);
                }
            }
            KeyCode::Char('i') => {
                self.details = !self.details;
                if self.details && !self.pane {
                    self.say(format!("details need a terminal at least {PANE_MIN_WIDTH} columns wide"), DIM);
                }
            }
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
                Some((name, State::Stopped)) => return self.vm_job(name, Verb::Start),
                Some((name, state)) => self.say(format!("{name} is already {state}"), DIM),
                None => {}
            },
            KeyCode::Char(c @ ('x' | 'X')) => match self.target() {
                Some((name, State::Stopped)) => self.say(format!("{name} is already stopped"), DIM),
                Some((name, _)) => return self.vm_job(name, if c == 'X' { Verb::ForceStop } else { Verb::Stop }),
                None => {}
            },
            KeyCode::Char('p') => match self.target() {
                Some((name, State::Running)) => return self.vm_job(name, Verb::Pause),
                Some((name, State::Paused)) => return self.vm_job(name, Verb::Resume),
                Some((name, state)) => self.say(format!("{name} is {state}; only running VMs can pause"), DIM),
                None => {}
            },
            KeyCode::Char('d') => {
                // A broken VM can't be loaded to delete it, so `target` says why instead.
                if let Some((name, _)) = self.target() {
                    self.modal = Some(Modal::DeleteVm(name));
                }
            }
            _ => {}
        }
        Action::None
    }

    /// A job on VM `vm`, which shows as busy until it ends and then says `done`.
    fn vm_task(
        &mut self,
        vm: String,
        verb: Verb,
        args: Vec<String>,
        then: Option<Vec<String>>,
        done: String,
    ) -> Action {
        let target = Target::Vm(vm);
        self.busy.insert(target.clone(), Busy { verb, done: false, pending: None });
        Action::Job(Job { target, verb, args, then, done: Some(done) })
    }

    /// Save a snapshot of `vm` right away, under the next free `snap-N` name.
    fn quick_snap(&mut self, vm: String) -> Action {
        let mut args = vec!["snap".to_string(), vm.clone()];
        let done = match self.history_of(&vm).map(History::next_name) {
            Some(name) => {
                args.push(name.clone());
                format!("✓ saved {vm} as {name}")
            }
            None => format!("✓ saved a snapshot of {vm}"),
        };
        self.vm_task(vm, Verb::Snapshot, args, None, done)
    }

    fn snap_key(&mut self, key: KeyEvent) -> Action {
        let vm = self.snap_vm.clone();
        let at = self.snap_index();
        let Some(history) = self.snap_history() else { return Action::None };
        let len = history.entries.len();
        let moved = match key.code {
            _ if len == 0 => None,
            KeyCode::Up | KeyCode::Char('k') => at.map(|i| i.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => at.map(|i| (i + 1).min(len - 1)),
            KeyCode::Home | KeyCode::Char('g') => Some(0),
            KeyCode::End | KeyCode::Char('G') => Some(len - 1),
            _ => None,
        };
        if let Some(i) = moved {
            self.snap_selected = Some(history.entries[i].snap.name.clone());
            return Action::None;
        }
        let pick = self.snap_pick().map(|p| p.snap.name.clone());
        let taken: Vec<String> = history.entries.iter().map(|e| e.snap.name.clone()).collect();
        let suggested = history.next_name();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(busy) = self.busy.get(&Target::Vm(vm.clone()))
            && matches!(key.code, KeyCode::Char('c' | 'd' | 's') | KeyCode::Enter)
        {
            self.say(format!("{vm} is busy {}", busy.verb.doing()), WARN);
            return Action::None;
        }
        match (key.code, pick) {
            (KeyCode::Char('s'), _) if ctrl => return self.quick_snap(vm),
            (KeyCode::Char('c'), _) => self.modal = Some(Modal::Snap(Box::new(SnapForm::new(&vm, &suggested, taken)))),
            (KeyCode::Enter, Some(name)) => self.modal = Some(Modal::Restore(name)),
            (KeyCode::Char('d'), Some(name)) => self.modal = Some(Modal::DeleteSnapshot(name)),
            _ => {}
        }
        Action::None
    }

    fn image_key(&mut self, key: KeyEvent) -> Action {
        if navigate(&mut self.image_table, key.code, self.images.len()) {
            return Action::None;
        }
        if key.code == KeyCode::Char('a') {
            self.modal = Some(Modal::AddImage(Box::new(AddImageForm::new(&self.home))));
            return Action::None;
        }
        let Some(info) = self.selected_image() else {
            if key.code == KeyCode::Char('n') {
                self.open_new(None);
            }
            return Action::None;
        };
        let (name, size, on_disk, custom) = (info.name.clone(), info.size, info.on_disk, info.custom);
        let downloading = info.downloading.is_some();
        let native = info.native;
        if let Some(busy) = self.busy.get(&Target::Image(name.clone()))
            && matches!(key.code, KeyCode::Char('p' | 'd'))
        {
            self.say(format!("{name} is busy {}", busy.verb.doing()), WARN);
            return Action::None;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('n') => self.open_new(Some(name)),
            KeyCode::Char('p') if custom || on_disk > 0 => self.say(format!("{name} is already on disk"), DIM),
            KeyCode::Char('p') if downloading => self.say(format!("{name} is already downloading"), DIM),
            KeyCode::Char('p') if !native => {
                self.say(format!("{name} has no {} build", self.host), WARN);
            }
            KeyCode::Char('p') => {
                let args = vec!["images".into(), "pull".into(), name.clone()];
                return self.job(Target::Image(name), Verb::Pull, args);
            }
            KeyCode::Char('d') if !custom && on_disk == 0 => self.say(format!("{name} isn't downloaded"), DIM),
            KeyCode::Char('d') => self.modal = Some(Modal::DeleteImage { name, size: on_disk.max(size), custom }),
            _ => {}
        }
        Action::None
    }

    fn draw(&mut self, frame: &mut Frame) {
        if self.status.as_ref().is_some_and(|s| s.at.elapsed() > STATUS_FOR) {
            self.status = None;
        }
        let area = frame.area();
        let counts = self.counts();
        // The message gets what's left of the top border after the tabs and the counts.
        let room = (area.width as usize).saturating_sub(counts.width() + 24);
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(BORDER)
            .title(self.title(room))
            .title(counts.right_aligned())
            .title_bottom(self.hints(area.width));
        if self.error.is_some() && self.view == View::Vms {
            block = block.border_style(Style::new().fg(Color::Red));
        }
        let inner = block.inner(area);
        frame.render_widget(block, area);
        // One column of breathing room inside the box.
        let body = inner.inner(Margin { horizontal: 1, vertical: 0 });

        let spinner = SPINNER[self.tick % SPINNER.len()];
        // The selected VM's details on the right, when there's room for both.
        self.pane = self.details
            && self.view != View::Images
            && body.width >= PANE_MIN_WIDTH - 4
            && self.vms.as_ref().is_some_and(|vms| !vms.is_empty());
        let (body, gap) = if self.pane {
            let width = (body.width * 2 / 5).clamp(40, 64);
            let [table, gap, pane] =
                Layout::horizontal([Constraint::Fill(1), Constraint::Length(2), Constraint::Length(width)]).areas(body);
            if self.view == View::Snapshots {
                self.draw_snap_details(frame, pane, spinner);
            } else {
                self.draw_details(frame, pane, spinner);
            }
            (table, Some(gap))
        } else {
            (body, None)
        };
        // The snapshot list's legend at the bottom, a blank line clear of the keys in the border.
        let body = if self.view == View::Snapshots && body.height > 5 {
            let [list, legend, _] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(1), Constraint::Length(1)]).areas(body);
            frame.render_widget(Paragraph::new(snapshot_legend()), legend);
            list
        } else {
            body
        };
        let rows = match self.view {
            View::Vms => match &self.vms {
                None => {
                    frame.render_widget(Paragraph::new(Line::styled("loading…", DIM)), body);
                    0
                }
                Some(vms) if vms.is_empty() && self.pending().is_empty() => {
                    draw_empty(frame, body);
                    0
                }
                Some(vms) => {
                    let table = self.vm_table(vms, spinner);
                    frame.render_stateful_widget(table, body, &mut self.table);
                    vms.len()
                }
            },
            View::Images => {
                let table = self.image_table(spinner);
                frame.render_stateful_widget(table, body, &mut self.image_table);
                self.images.len()
            }
            View::Snapshots => match &self.history {
                Some((name, Err(e))) if *name == self.snap_vm => {
                    frame.render_widget(Paragraph::new(Line::styled(format!("✗ {e}"), ERROR)), body);
                    0
                }
                _ if self.snap_history().is_none() => {
                    frame.render_widget(Paragraph::new(Line::styled("loading…", DIM)), body);
                    0
                }
                _ if self.snap_history().is_some_and(|h| h.entries.is_empty()) => {
                    draw_no_snapshots(frame, body, &self.snap_vm);
                    0
                }
                _ => {
                    self.snap_table.select(self.snap_index());
                    let table = self.snap_table(spinner);
                    frame.render_stateful_widget(table, body, &mut self.snap_table);
                    self.snap_history().map_or(0, |h| h.entries.len())
                }
            },
        };
        // A scrollbar on the box's edge when the rows don't all fit below the header.
        let visible = body.height.saturating_sub(1) as usize;
        if rows > visible {
            let offset = match self.view {
                View::Vms => self.table.offset(),
                View::Images => self.image_table.offset(),
                View::Snapshots => self.snap_table.offset(),
            };
            let mut scroll =
                ScrollbarState::new(rows.saturating_sub(visible)).position(offset).viewport_content_length(visible);
            let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("┃")
                .track_style(BORDER)
                .thumb_style(Style::new().fg(Color::Cyan));
            // On the box's edge, or between the table and the details.
            let edge = gap.map_or(area, |gap| Rect { width: 1, ..gap });
            let track = Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..edge };
            frame.render_stateful_widget(bar, track, &mut scroll);
        }

        match &mut self.modal {
            Some(Modal::Help) => draw_help(frame, area),
            Some(Modal::DeleteVm(name)) => {
                let what = vec![Span::raw("Delete "), Span::styled(name.clone(), HEADER), Span::raw(" and its disk?")];
                draw_confirm(frame, area, what, "Cached images and snapshots go with it.");
            }
            Some(Modal::DeleteImage { name, size, custom }) => {
                let what = vec![
                    Span::raw("Delete "),
                    Span::styled(name.clone(), HEADER),
                    Span::raw(format!(" ({})?", bytes(*size))),
                ];
                let note = if *custom {
                    "It was added by you; the original file isn't touched."
                } else {
                    "VMs keep their own disks; you can download it again."
                };
                draw_confirm(frame, area, what, note);
            }
            Some(Modal::New(form)) => {
                // The form brings its own margin.
                let inner = dialog(frame, area, 80, form.height() + 4, "new VM", BORDER, 0);
                form.render(frame, inner);
            }
            Some(Modal::AddImage(form)) => {
                let inner = dialog(frame, area, 72, AddImageForm::HEIGHT + 4, "add image", BORDER, 2);
                form.render(frame, inner);
            }
            Some(Modal::Snap(form)) => {
                let inner = dialog(frame, area, 72, SnapForm::HEIGHT + 4, "snapshot", BORDER, 2);
                form.render(frame, inner);
            }
            Some(Modal::Ports(vm, form)) => {
                let inner = dialog(frame, area, 72, form.height() + 4, &format!("{vm}'s ports"), BORDER, 2);
                form.render(frame, inner);
            }
            Some(Modal::Restore(name)) => {
                let what = vec![
                    Span::raw("Take "),
                    Span::styled(self.snap_vm.clone(), HEADER),
                    Span::raw(" back to "),
                    Span::styled(name.clone(), HEADER),
                    Span::raw("?"),
                ];
                let note = "What it has now is lost unless you save it first. Other snapshots are kept.";
                let keys = [("y", "go back"), ("s", "save first, then go back"), ("n", "cancel")];
                draw_ask(frame, area, "go back", WARN, what, note, &keys);
            }
            Some(Modal::DeleteSnapshot(name)) => {
                let what = vec![Span::raw("Delete snapshot "), Span::styled(name.clone(), HEADER), Span::raw("?")];
                draw_confirm(frame, area, what, "The VM and its other snapshots aren't touched.");
            }
            None => {}
        }
    }

    /// `vx`, the two views with the current one bold, then the latest message (cut to `room`).
    fn title(&self, room: usize) -> Line<'static> {
        let tab = |name: &'static str, view: View| {
            if self.view == view { Span::styled(name, HEADER) } else { Span::styled(name, DIM) }
        };
        let mut spans = vec![Span::styled("─ ", BORDER), Span::styled("vx", ACCENT), Span::styled(" ─ ", BORDER)];
        if self.view == View::Snapshots {
            spans.extend([
                Span::styled("VMs", DIM),
                Span::styled(" › ", BORDER),
                Span::styled(self.snap_vm.clone(), HEADER),
                Span::styled(" › ", BORDER),
                Span::styled("snapshots", HEADER),
            ]);
        } else {
            spans.extend([tab("VMs", View::Vms), Span::styled(" · ", BORDER), tab("images", View::Images)]);
        }
        spans.push(Span::raw(" "));
        if let Some(status) = &self.status
            && room > 4
        {
            spans.push(Span::styled("─ ", BORDER));
            spans.push(Span::styled(clip(&status.text, room), status.style));
            spans.push(Span::raw(" "));
        }
        Line::from(spans)
    }

    /// Totals for the current view, in the top border.
    fn counts(&self) -> Line<'static> {
        let mut spans = Vec::new();
        match self.view {
            View::Vms => {
                if let Some(error) = &self.error {
                    spans.push(Span::styled(format!(" {error}"), ERROR));
                } else if let Some(vms) = &self.vms {
                    let running = vms.iter().filter(|e| matches!(e.vm, Ok((_, State::Running)))).count();
                    let plural = if vms.len() == 1 { "" } else { "s" };
                    spans.push(Span::styled(format!(" {} VM{plural} · ", vms.len()), DIM));
                    spans.push(Span::styled(format!("{running} running"), if running > 0 { OK } else { DIM }));
                }
            }
            View::Images => {
                let kept = self.images.iter().filter(|i| i.on_disk > 0).count();
                let total: u64 = self.images.iter().map(|i| i.on_disk).sum();
                spans.push(Span::styled(
                    format!(" {} images · {kept} on disk · {}", self.images.len(), bytes(total)),
                    DIM,
                ));
            }
            View::Snapshots => {
                if let Some(h) = self.snap_history() {
                    let n = h.entries.len();
                    let plural = if n == 1 { "" } else { "s" };
                    spans.push(Span::styled(format!(" {n} snapshot{plural}"), DIM));
                    if let Some(at) = &h.current {
                        spans.push(Span::styled(format!(" · at {}", clip(at, 20)), DIM));
                    }
                }
            }
        }
        if !spans.is_empty() {
            spans.push(Span::styled(" ─", BORDER));
        }
        Line::from(spans)
    }

    /// Keys for what the selection can do, in the bottom border `width` wide.
    fn hints(&self, width: u16) -> Line<'static> {
        let mut spans = vec![Span::styled("─ ", BORDER)];
        let mut keys: Vec<(&str, &str)> = Vec::new();
        match self.view {
            View::Vms => match self.selected().map(|e| (&e.name, &e.vm)) {
                Some((name, Err(e))) => {
                    spans.push(Span::styled(format!("{name}: {e}"), ERROR));
                    spans.push(Span::styled(" · ", BORDER));
                }
                Some((name, Ok((_, state)))) if !self.busy.contains_key(&Target::Vm(name.clone())) => {
                    keys.push(("⏎", "ssh"));
                    match state {
                        State::Stopped => keys.extend([("s", "start"), ("d", "delete")]),
                        State::Paused => keys.extend([("p", "resume"), ("x", "stop")]),
                        _ => keys.extend([("x", "stop"), ("p", "pause")]),
                    }
                    // The short label when the full one would push `q quit` off an 80-column screen.
                    keys.push(("S", if width >= 90 { "snapshots" } else { "snaps" }));
                }
                _ => {}
            },
            View::Images => {
                if let Some(info) = self.selected_image() {
                    keys.push(("⏎", "new VM"));
                    if !info.custom && info.on_disk == 0 && info.downloading.is_none() && info.native {
                        keys.push(("p", "download"));
                    }
                    if info.custom || info.on_disk > 0 {
                        keys.push(("d", "delete"));
                    }
                }
                keys.push(("a", "add"));
            }
            View::Snapshots => {
                if self.snap_pick().is_some() {
                    keys.extend([("⏎", "go back to it"), ("d", "delete")]);
                }
                keys.extend([("c", "save"), ("esc", "back")]);
            }
        }
        if self.view == View::Vms {
            keys.push(("n", "new"));
        }
        if self.view != View::Snapshots {
            let other = if self.view == View::Vms { "images" } else { "VMs" };
            keys.push(("tab", other));
        }
        keys.extend([("?", "keys"), ("q", "quit")]);
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

    /// VMs being created that don't have a directory yet, by name.
    fn pending(&self) -> Vec<(&str, &Pending)> {
        let exists = |name: &str| self.vms.iter().flatten().any(|e| e.name == name);
        let mut pending: Vec<(&str, &Pending)> = self
            .busy
            .iter()
            .filter_map(|(target, busy)| match (target, &busy.pending) {
                (Target::Vm(name), Some(p)) if !busy.done && !exists(name) => Some((name.as_str(), p)),
                _ => None,
            })
            .collect();
        pending.sort_by_key(|(name, _)| *name);
        pending
    }

    /// `↓ 30%` while `image` downloads.
    fn download_progress(&self, image: &str) -> Option<String> {
        let info = self.images.iter().find(|i| i.name == image)?;
        let done = info.downloading?;
        // The catalog's size is approximate, so never claim 100% before it's done.
        Some(format!("↓ {}%", (done * 100 / info.size.max(1)).min(99)))
    }

    /// What to show in place of a busy VM's state.
    fn vm_busy_label(&self, name: &str) -> Option<String> {
        let busy = self.busy.get(&Target::Vm(name.into()))?;
        if busy.verb != Verb::Create {
            return Some(busy.verb.doing().into());
        }
        if let Some(entry) = self.vms.iter().flatten().find(|e| e.name == name) {
            return Some(if entry.setting_up { "installing" } else { "booting" }.into());
        }
        let image = busy.pending.as_ref().map(|p| p.image.as_str()).unwrap_or_default();
        Some(self.download_progress(image).unwrap_or_else(|| "creating".into()))
    }

    /// The selected VM in its own box: what it is, and live numbers from inside it while it runs.
    fn draw_details(&self, frame: &mut Frame, area: Rect, spinner: char) {
        let Some(entry) = self.selected() else { return };
        let title = Line::from(vec![
            Span::styled("─ ", BORDER),
            Span::styled(clip(&entry.name, area.width.saturating_sub(6) as usize), ACCENT),
            Span::raw(" "),
        ]);
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(BORDER).title(title);
        let inner = block.inner(area).inner(Margin { horizontal: 1, vertical: 0 });
        frame.render_widget(block, area);
        let (spec, state) = match &entry.vm {
            Ok(vm) => vm,
            Err(e) => {
                let text = vec![Line::styled("✗ broken", ERROR), Line::default(), Line::styled(e.clone(), DIM)];
                frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), inner);
                return;
            }
        };
        let width = inner.width as usize;
        let stats = self.stats.get(&entry.name).filter(|_| *state == State::Running);
        let busy = self.vm_busy_label(&entry.name);
        let lines = |cpu_rows: usize, net_rows: usize| {
            let mut lines = Vec::new();
            let mut first = vec![];
            match &busy {
                Some(label) => first.push(Span::styled(format!("{spinner} {label}"), BUSY)),
                None => first.extend(state_line(state).spans),
            }
            if let Some(stats) = stats {
                first.push(Span::styled(format!(" · up {}", uptime(stats.latest.uptime)), DIM));
            }
            lines.push(Line::from(first));
            let about = format!("{} · {} · {} CPUs · {}", spec.image, spec.arch, spec.cpus, spec.memory);
            lines.push(Line::styled(clip(&about, width), DIM));
            lines.push(Line::default());
            let field = |key: &str, value: String| {
                Line::from(vec![Span::styled(format!("{key:6}"), DIM), Span::raw(clip(&value, width - 6))])
            };
            lines.push(field("ssh", format!("127.0.0.1:{}", spec.ssh.port)));
            let ports = match spec.forwards().map(|(h, g)| format!("{h} → {g}")).collect::<Vec<_>>() {
                ports if ports.is_empty() => "none · f to forward one".to_string(),
                ports => format!("{} · f to change", ports.join(", ")),
            };
            lines.push(field("ports", ports));
            if !spec.mounts.is_empty() {
                let mounts: Vec<&str> = spec.mounts.iter().map(|m| m.guest.as_str()).collect();
                lines.push(field("mount", mounts.join(", ")));
            }
            if let Some(disk) = entry.disk {
                lines.push(field("file", format!("{} on host", bytes(disk))));
            }
            if let Some(h) = self.history_of(&entry.name) {
                lines.push(Line::default());
                lines.extend(recent_snapshots(h, width));
            }
            lines.push(Line::default());
            let Some(stats) = stats else {
                let why = match state {
                    _ if busy.is_some() => String::new(),
                    State::Running => format!("{spinner} connecting…"),
                    State::Stopped => "live numbers show while it runs".into(),
                    State::Paused => "paused · p resumes it".into(),
                    State::Other(_) => String::new(),
                };
                lines.push(Line::styled(why, DIM));
                return lines;
            };
            let s = &stats.latest;
            lines.push(heading("CPU", format!("{:.0}%", stats.cpu * 100.0), width));
            lines.extend(graph(&stats.cpu_history, 1.0, width, cpu_rows, heat));
            // Cores two to a line, when they fit.
            let columns = if width >= 36 { 2 } else { 1 };
            let column = (width + 2) / columns - 2;
            for chunk in stats.cores.chunks(columns).enumerate() {
                let (row, cores) = chunk;
                let mut spans = Vec::new();
                for (i, usage) in cores.iter().enumerate() {
                    if i > 0 {
                        spans.push(Span::raw("  "));
                    }
                    let label = format!("{:<3}", format!("c{}", row * columns + i));
                    spans.push(Span::styled(label, DIM));
                    spans.extend(meter(*usage, column.saturating_sub(8)));
                    spans.push(Span::raw(format!("{:>5}", format!("{:.0}%", usage * 100.0))));
                }
                lines.push(Line::from(spans));
            }
            let [one, five, fifteen] = s.load;
            lines.push(Line::styled(format!("load {one:.2} {five:.2} {fifteen:.2}"), DIM));
            lines.push(Line::default());

            let mut gauges = vec![("MEM", s.mem_total.saturating_sub(s.mem_available), s.mem_total)];
            if s.swap_total > 0 {
                gauges.push(("SWAP", s.swap_total.saturating_sub(s.swap_free), s.swap_total));
            }
            gauges.push(("DISK", s.disk_used, s.disk_size));
            let values: Vec<String> =
                gauges.iter().map(|(_, used, total)| format!("{} / {}", amount(*used), amount(*total))).collect();
            let value_width = values.iter().map(|v| v.chars().count()).max().unwrap_or(0);
            for ((label, used, total), value) in gauges.into_iter().zip(values) {
                let ratio = if total == 0 { 0.0 } else { used as f64 / total as f64 };
                let mut spans = vec![Span::styled(format!("{label:5}"), HEADER)];
                spans.extend(meter(ratio, width.saturating_sub(5 + 1 + value_width)));
                spans.push(Span::raw(format!(" {value:>value_width$}")));
                lines.push(Line::from(spans));
            }
            lines.push(Line::default());

            // Both graphs on one scale, so they compare at a glance.
            let peak = stats.rx_history.iter().chain(&stats.tx_history).fold(1000.0_f64, |a, b| a.max(*b));
            for (label, rate, history, color) in [
                ("NET ↓", stats.rx, &stats.rx_history, Color::Cyan),
                ("NET ↑", stats.tx, &stats.tx_history, Color::Magenta),
            ] {
                lines.push(heading(label, format!("{}/s", amount(rate as u64)), width));
                lines.extend(graph(history, peak, width, net_rows, |_| Style::new().fg(color)));
            }
            lines
        };
        // Graphs get the rows left over: CPU first, then up to three each for the network.
        let spare = (inner.height as usize).saturating_sub(lines(0, 0).len());
        let cpu_rows = (spare * 3 / 5).clamp(1.min(spare), 8);
        let net_rows = ((spare - cpu_rows) / 2).min(3);
        frame.render_widget(Paragraph::new(lines(cpu_rows, net_rows)), inner);
    }

    /// The snapshots as a list, oldest first, like save slots, with the current one highlighted.
    fn snap_table(&self, spinner: char) -> Table<'static> {
        let Some(h) = self.snap_history() else { return Table::default() };
        let selected = self.snap_table.selected();
        let right = |s: String| Cell::from(Line::from(s).alignment(Alignment::Right));
        let busy = self.vm_busy_label(&self.snap_vm);
        let rows: Vec<Row> = h
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                // The selection is the row's own style rather than the table's highlight, which would
                // paint over the current snapshot's label.
                let text = if selected == Some(i) { SELECTED.patch(SELECTED_TEXT) } else { Style::new() };
                let here = h.current.as_deref() == Some(entry.snap.name.as_str());
                let marker = marker(entry);
                let name = name_label(&entry.snap.name, 24, here);
                let memory = match entry.snap.memory {
                    0 => Span::styled("disk", DIM),
                    n => Span::raw(bytes(n)),
                };
                // Where it was saved from, when that isn't the row above; and while a job runs,
                // what it's doing, on the current snapshot's row.
                let mut note = Vec::new();
                match &busy {
                    Some(busy) if here => note.push(Span::styled(format!("{spinner} {busy}"), BUSY)),
                    _ => {
                        if let Some(from) = h.from(i) {
                            let sep = if entry.note.is_empty() { "" } else { " · " };
                            note.push(Span::styled(format!("from {from}{sep}"), DIM));
                        }
                        note.push(Span::styled(entry.note.clone(), DIM));
                    }
                }
                Row::new(vec![
                    Cell::from(Line::from(vec![marker, Span::raw(" "), name])),
                    right(snapshot::ago(entry.snap.created)),
                    Cell::from(Line::from(memory).alignment(Alignment::Right)),
                    Cell::from(Line::from(note)),
                ])
                .style(text)
            })
            .collect();
        // The marker, a space, and the name with a space either side.
        let name_width = h.entries.iter().map(|e| e.snap.name.chars().count().min(24)).max().unwrap_or(0) + 4;
        let widths = [
            Constraint::Length(name_width.max(8) as u16),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Fill(1),
        ];
        let header =
            Row::new(vec![Cell::from("SNAPSHOT"), right("SAVED".into()), right("MEMORY".into()), Cell::from("NOTE")])
                .style(HEADER);
        Table::new(rows, widths).header(header).column_spacing(2)
    }

    fn vm_state(&self, name: &str) -> Option<&State> {
        self.vms.iter().flatten().find(|e| e.name == name).and_then(|e| e.vm.as_ref().ok()).map(|(_, state)| state)
    }

    /// The selected snapshot in the details pane.
    fn draw_snap_details(&self, frame: &mut Frame, area: Rect, spinner: char) {
        let Some(h) = self.snap_history() else { return };
        let vm = &self.snap_vm;
        let entry = self.snap_pick();
        let name = entry.map_or(vm.clone(), |e| e.snap.name.clone());
        let title = Line::from(vec![
            Span::styled("─ ", BORDER),
            Span::styled(clip(&name, area.width.saturating_sub(6) as usize), ACCENT),
            Span::raw(" "),
        ]);
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(BORDER).title(title);
        let inner = block.inner(area).inner(Margin { horizontal: 1, vertical: 0 });
        frame.render_widget(block, area);
        let width = inner.width as usize;
        let field = |key: &str, value: String| {
            Line::from(vec![Span::styled(format!("{key:7}"), DIM), Span::raw(clip(&value, width.saturating_sub(7)))])
        };
        let state = match self.vm_busy_label(vm) {
            Some(busy) => Line::styled(format!("{spinner} {busy}"), BUSY),
            None => self.vm_state(vm).map(state_line).unwrap_or_default(),
        };
        let mut lines = Vec::new();
        let Some(e) = entry else {
            lines.extend([state, Line::default(), Line::styled("no snapshots yet", DIM)]);
            frame.render_widget(Paragraph::new(lines), inner);
            return;
        };
        if h.current.as_deref() == Some(e.snap.name.as_str()) {
            let mut here = vec![Span::styled("current snapshot · ", ACCENT)];
            here.extend(state.spans);
            lines.push(Line::from(here));
        } else {
            lines.push(Line::styled("not the current snapshot", DIM));
        }
        lines.push(Line::default());
        let kind = match e.snap.memory {
            0 => "disk only".to_string(),
            n => format!("memory and disk · {} of memory", bytes(n)),
        };
        lines.push(field("saved", kind));
        lines.push(field("", format!("{} · {}", snapshot::ago(e.snap.created), local_time(e.snap.created))));
        lines.push(field("from", e.parent.clone().unwrap_or_else(|| "the start".into())));
        if !e.note.is_empty() {
            lines.push(Line::default());
            lines.push(Line::raw(e.note.clone()));
        }
        lines.push(Line::default());
        let back = if e.snap.memory > 0 {
            "going back resumes it right where it was"
        } else {
            "going back boots it from this disk"
        };
        lines.push(Line::styled(back, DIM));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    /// The selected row is a dark bar; its text is forced to white so it reads on light themes too.
    fn vm_table(&self, vms: &[Entry], spinner: char) -> Table<'static> {
        let selected = self.table.selected();
        let right = |s: String| Cell::from(Line::from(s).alignment(Alignment::Right));
        let mut rows: Vec<Row> = vms
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let text = if selected == Some(i) { SELECTED_TEXT } else { Style::new() };
                let quiet = if selected == Some(i) { SELECTED_TEXT } else { DIM };
                let row = match &entry.vm {
                    Ok((spec, state)) => {
                        let state = match self.vm_busy_label(&entry.name) {
                            Some(label) => Line::styled(format!("{spinner} {label}"), BUSY),
                            None => state_line(state),
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
                    Err(_) => {
                        vec![Cell::from(clip(&entry.name, NAME_MAX)), Cell::from(Span::styled("✗ broken", ERROR))]
                    }
                };
                Row::new(row).style(text)
            })
            .collect();
        // VMs still being created come last, until their directory appears.
        for (name, p) in self.pending() {
            let label = self.vm_busy_label(name).unwrap_or_default();
            rows.push(Row::new(vec![
                Cell::from(clip(name, NAME_MAX)),
                Cell::from(Line::styled(format!("{spinner} {label}"), BUSY)),
                Cell::from(clip(&p.image, IMAGE_MAX)),
                Cell::from(p.arch.to_string()),
                right(p.cpus.clone()),
                right(p.memory.clone()),
                Cell::from(""),
            ]));
        }
        let pending = self.pending();
        let names = vms.iter().map(|e| e.name.chars().count()).chain(pending.iter().map(|(n, _)| n.chars().count()));
        let images = vms
            .iter()
            .map(|e| e.vm.as_ref().map_or(0, |(s, _)| s.image.chars().count()))
            .chain(pending.iter().map(|(_, p)| p.image.chars().count()));
        let widths = [
            Constraint::Length(names.max().unwrap_or(0).clamp(4, NAME_MAX) as u16),
            Constraint::Length(12), // "⠧ installing"
            Constraint::Length(images.max().unwrap_or(0).clamp(5, IMAGE_MAX) as u16),
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

    fn image_table(&self, spinner: char) -> Table<'static> {
        let selected = self.image_table.selected();
        let right = |s: String| Cell::from(Line::from(s).alignment(Alignment::Right));
        let mut rows: Vec<Row> = self
            .images
            .iter()
            .enumerate()
            .map(|(i, info)| {
                let text = if selected == Some(i) { SELECTED_TEXT } else { Style::new() };
                let busy = self.busy.get(&Target::Image(info.name.clone()));
                let on_disk = match (busy, self.download_progress(&info.name)) {
                    (_, Some(progress)) => Line::styled(format!("{spinner} {progress}"), BUSY),
                    // Fetching the checksum, before the first bytes arrive.
                    (Some(busy), None) if busy.verb == Verb::Pull => Line::styled(format!("{spinner} ↓ 0%"), BUSY),
                    (Some(busy), None) => Line::styled(format!("{spinner} {}", busy.verb.doing()), BUSY),
                    (None, None) if info.on_disk > 0 => Line::styled(bytes(info.on_disk), OK),
                    (None, None) => Line::styled("-", DIM),
                };
                let note = if info.name == image::DEFAULT {
                    Span::styled("default", DIM)
                } else if info.custom {
                    Span::styled("custom", BUSY)
                } else if !info.native {
                    Span::styled(format!("{} only", other(self.host)), WARN)
                } else {
                    Span::raw("")
                };
                let size = if info.custom { bytes(info.size) } else { format!("~{}", bytes(info.size)) };
                Row::new(vec![
                    Cell::from(clip(&info.name, IMAGE_NAME_MAX)),
                    Cell::from(clip(&info.title, 20)),
                    right(size),
                    Cell::from(on_disk),
                    Cell::from(note),
                ])
                .style(text)
            })
            .collect();
        // Images being added come last, until they've been copied.
        let mut adding: Vec<&str> = self
            .busy
            .iter()
            .filter(|(t, b)| b.verb == Verb::AddImage && !self.images.iter().any(|i| i.name == t.name()))
            .map(|(t, _)| t.name())
            .collect();
        adding.sort();
        for name in adding {
            rows.push(Row::new(vec![
                Cell::from(clip(name, IMAGE_NAME_MAX)),
                Cell::from("added by you"),
                Cell::from(""),
                Cell::from(Line::styled(format!("{spinner} copying"), BUSY)),
                Cell::from(Span::styled("custom", BUSY)),
            ]));
        }
        let name_w = self.images.iter().map(|i| i.name.chars().count()).max().unwrap_or(0).clamp(5, IMAGE_NAME_MAX);
        let widths = [
            Constraint::Length(name_w as u16),
            Constraint::Length(20),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Fill(1),
        ];
        let header = Row::new(vec![
            Cell::from("IMAGE"),
            Cell::from("DESCRIPTION"),
            right("SIZE".into()),
            Cell::from("ON DISK"),
            Cell::from(""),
        ])
        .style(HEADER);
        Table::new(rows, widths).header(header).column_spacing(2).row_highlight_style(SELECTED)
    }
}

/// ↑↓ j k, Home End g G. Returns whether the key moved the selection.
fn navigate(table: &mut TableState, code: KeyCode, len: usize) -> bool {
    if len == 0 {
        return matches!(code, KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k'));
    }
    let next = |i: Option<usize>| Some(i.map_or(0, |i| (i + 1).min(len - 1)));
    match code {
        KeyCode::Up | KeyCode::Char('k') => table.select_previous(),
        KeyCode::Down | KeyCode::Char('j') => table.select(next(table.selected())),
        KeyCode::Home | KeyCode::Char('g') => table.select_first(),
        KeyCode::End | KeyCode::Char('G') => table.select(Some(len - 1)),
        _ => return false,
    }
    true
}

/// The row to select after the list changed: the same item if it's still there, otherwise
/// the same position, kept in range.
fn keep_in_range(found: Option<usize>, before: Option<usize>, len: usize) -> Option<usize> {
    match (found, before) {
        (Some(i), _) => Some(i),
        (None, _) if len == 0 => None,
        (None, Some(i)) => Some(i.min(len - 1)),
        (None, None) => Some(0),
    }
}

fn other(arch: Arch) -> Arch {
    match arch {
        Arch::Aarch64 => Arch::X86_64,
        Arch::X86_64 => Arch::Aarch64,
    }
}

fn state_line(state: &State) -> Line<'static> {
    let (dot, color) = match state {
        State::Running => ("●", OK),
        State::Stopped => ("○", DIM),
        _ => ("◐", WARN),
    };
    Line::from(vec![Span::styled(dot, color), Span::styled(format!(" {state}"), color)])
}

/// The `vx new` arguments for what the form produced.
fn new_argv(args: &NewArgs) -> Vec<String> {
    let mut argv = vec!["new".to_string(), args.name.clone().unwrap_or_default()];
    argv.extend(["--image".into(), args.image.clone(), "--disk".into(), args.disk.clone()]);
    if let Some(cpus) = args.cpus {
        argv.extend(["--cpus".into(), cpus.to_string()]);
    }
    if let Some(mem) = &args.mem {
        argv.extend(["--mem".into(), mem.clone()]);
    }
    if let Some(arch) = args.arch {
        argv.extend(["--arch".into(), arch.to_string()]);
    }
    if args.bare {
        argv.push("--bare".into());
    }
    if !args.install.is_empty() {
        argv.extend(["--install".into(), args.install.join(",")]);
    }
    if let Some(setup) = &args.setup {
        argv.extend(["--setup".into(), setup.clone()]);
    }
    for m in &args.mount {
        argv.extend(["--mount".into(), m.clone()]);
    }
    argv
}

/// Run `vx <args>` on a thread, in its own process group so Ctrl-C in an ssh session can't
/// interrupt it, and report how it went.
fn spawn_job(io: &Io, job: Job) {
    let (tx, exe, home) = (io.tx.clone(), io.exe.clone(), io.home.clone());
    thread::spawn(move || {
        let run = |args: &[String]| {
            let output =
                Command::new(&exe).args(args).env("VX_HOME", &home).stdin(Stdio::null()).process_group(0).output();
            match output {
                Ok(out) if out.status.success() => Ok(()),
                Ok(out) => Err(error_line(&String::from_utf8_lossy(&out.stderr))),
                Err(e) => Err(e.to_string()),
            }
        };
        let result = run(&job.args).and_then(|()| job.then.as_deref().map_or(Ok(()), run));
        let _ = tx.send(Msg::Done { target: job.target, verb: job.verb, result, done: job.done });
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

/// The narrowest terminal that shows the details pane beside the VM table.
const PANE_MIN_WIDTH: u16 = 120;
/// Meter cells that aren't filled.
const EMPTY: Style = Style::new().fg(Color::Indexed(238));

/// btop's colors: green while there's plenty of room, then yellow, then red.
fn heat(level: f64) -> Style {
    Style::new().fg(match level {
        ..0.5 => Color::Green,
        ..0.8 => Color::Yellow,
        _ => Color::Red,
    })
}

/// `label` on the left and `value` on the right of a `width`-wide line.
fn heading(label: &str, value: String, width: usize) -> Line<'static> {
    let pad = width.saturating_sub(label.chars().count() + value.chars().count());
    Line::from(vec![Span::styled(label.to_string(), HEADER), Span::raw(" ".repeat(pad)), Span::raw(value)])
}

/// `■■■■■■□□□□`: `width` cells, the filled ones colored by how far along they are.
fn meter(ratio: f64, width: usize) -> Vec<Span<'static>> {
    let filled = (ratio.clamp(0.0, 1.0) * width as f64).round() as usize;
    let mut spans: Vec<Span> = Vec::new();
    for i in 0..width {
        let style = if i < filled { heat((i as f64 + 0.5) / width as f64) } else { EMPTY };
        match spans.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push('■'),
            _ => spans.push(Span::styled("■", style)),
        }
    }
    spans
}

/// `rows` lines of bars, newest on the right, scaled so `max` fills them. `color` gets each
/// row's height, from 0 at the bottom to 1 at the top.
fn graph(
    values: &VecDeque<f64>,
    max: f64,
    width: usize,
    rows: usize,
    color: impl Fn(f64) -> Style,
) -> Vec<Line<'static>> {
    const BARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let shown: Vec<f64> =
        values.iter().skip(values.len().saturating_sub(width)).map(|v| (v / max).clamp(0.0, 1.0)).collect();
    (0..rows)
        .map(|row| {
            let floor = (rows - 1 - row) as f64 / rows as f64;
            let mut text = " ".repeat(width - shown.len());
            for v in &shown {
                let level = ((v - floor) * rows as f64).clamp(0.0, 1.0);
                // Anything above zero shows on the bottom row, so a quiet VM isn't a blank graph.
                let index = if row == rows - 1 && *v > 0.0 {
                    ((level * 8.0).round() as usize).max(1)
                } else {
                    (level * 8.0).round() as usize
                };
                text.push(BARS[index]);
            }
            Line::styled(text, color((rows - row) as f64 / rows as f64 - 0.01))
        })
        .collect()
}

/// Like `bytes`, but small amounts don't round to `0 kB`.
fn amount(n: u64) -> String {
    if n < 1000 { format!("{n} B") } else { bytes(n) }
}

/// `2h 13m`
fn uptime(seconds: f64) -> String {
    let s = seconds as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d {}h", s / 86400, s % 86400 / 3600),
    }
}

/// Columns wider than this end in `…`, so one long name can't push the rest off screen.
const NAME_MAX: usize = 24;
const IMAGE_MAX: usize = 18;
const IMAGE_NAME_MAX: usize = 20;

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

/// `●` for a snapshot with memory, `○` for one of the disk alone, each in its own color.
fn marker(entry: &snapshot::Entry) -> Span<'static> {
    let style = if entry.snap.memory > 0 { LIVE } else { DISK };
    Span::styled(snapshot::marker(entry).to_string(), style)
}

/// A snapshot's name with a space either side, as a solid label if it's the current one.
fn name_label(name: &str, max: usize, here: bool) -> Span<'static> {
    Span::styled(format!(" {} ", clip(name, max)), if here { HERE } else { Style::new() })
}

/// What the snapshot markers and the highlight mean.
fn snapshot_legend() -> Line<'static> {
    Line::from(vec![
        Span::styled("●", LIVE),
        Span::styled(" resumes running   ", DIM),
        Span::styled("○", DISK),
        Span::styled(" boots from disk   ", DIM),
        Span::styled(" name ", HERE),
        Span::styled(" current snapshot", DIM),
    ])
}

/// The VM pane's snapshot section: the newest few, with the current one highlighted.
fn recent_snapshots(h: &History, width: usize) -> Vec<Line<'static>> {
    const SHOWN: usize = 3;
    let mut lines = vec![heading("SNAPSHOTS", h.entries.len().to_string(), width)];
    if h.entries.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("none yet · ", DIM),
            Span::styled("ctrl-s", ACCENT),
            Span::styled(" saves one", DIM),
        ]));
        return lines;
    }
    for e in h.entries.iter().rev().take(SHOWN) {
        let here = h.current.as_deref() == Some(e.snap.name.as_str());
        let when = snapshot::ago(e.snap.created);
        let name = name_label(&e.snap.name, width.saturating_sub(when.chars().count() + 6), here);
        let pad = width.saturating_sub(2 + name.width() + when.chars().count());
        lines.push(Line::from(vec![
            marker(e),
            Span::raw(" "),
            name,
            Span::raw(" ".repeat(pad)),
            Span::styled(when, DIM),
        ]));
    }
    let more = h.entries.len().saturating_sub(SHOWN);
    let mut hint = vec![Span::styled("S", ACCENT), Span::styled(" all", DIM)];
    if more > 0 {
        hint.push(Span::styled(format!(" ({more} more)"), DIM));
    }
    hint.extend([Span::styled(" · ", DIM), Span::styled("ctrl-s", ACCENT), Span::styled(" save one", DIM)]);
    lines.push(Line::from(hint));
    lines
}

fn draw_no_snapshots(frame: &mut Frame, area: Rect, vm: &str) {
    let [_, middle, _] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(3), Constraint::Fill(2)]).areas(area);
    let text = vec![
        Line::from(format!("{vm} has no snapshots yet")).alignment(Alignment::Center),
        Line::from(vec![
            Span::styled("press ", DIM),
            Span::styled("c", ACCENT),
            Span::styled(" to save one, so you can come back to this moment", DIM),
        ])
        .alignment(Alignment::Center),
    ];
    frame.render_widget(Paragraph::new(text), middle);
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

/// A rounded box of up to `width` × `height` in the middle of `area`, cleared, with `title`.
/// Returns the space inside it, after `pad` columns of padding on each side.
fn dialog(frame: &mut Frame, area: Rect, width: u16, height: u16, title: &str, border: Style, pad: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    let [row] = Layout::vertical([Constraint::Length(height)]).flex(Flex::Center).areas(area);
    let [rect] = Layout::horizontal([Constraint::Length(width)]).flex(Flex::Center).areas(row);
    frame.render_widget(Clear, rect);
    let title_style = if border == BORDER { ACCENT } else { border };
    let title =
        Line::from(vec![Span::styled("─ ", border), Span::styled(title.to_string(), title_style), Span::raw(" ")]);
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(border).title(title);
    let inner = block.inner(rect).inner(Margin { horizontal: pad, vertical: 1 });
    frame.render_widget(block, rect);
    inner
}

/// The help, in two columns that stack on narrow terminals.
const HELP: [&[(&str, &str)]; 2] = [
    &[
        ("", "VMs"),
        ("⏎", "ssh in, starting it first if needed"),
        ("c", "serial console · Ctrl-] returns"),
        ("l", "boot log · Ctrl-C returns"),
        ("s x X", "start · stop · force off"),
        ("p", "pause or resume"),
        ("n", "new VM"),
        ("d", "delete"),
        ("i", "show or hide details"),
        ("f", "forward ports"),
        ("S", "snapshots"),
        ("ctrl-s", "save a snapshot now"),
        ("", ""),
        ("", "images"),
        ("⏎", "new VM from it"),
        ("p", "download it now"),
        ("a", "add a disk image of your own"),
        ("d", "delete its copy"),
    ],
    &[
        ("", "snapshots"),
        ("c", "save one, with a name and note"),
        ("⏎", "go back to it"),
        ("d", "delete it"),
        ("esc", "back to VMs"),
        ("●", "resumes running: memory saved"),
        ("○", "boots from disk: disk only"),
        ("cyan", "the current snapshot"),
        ("", ""),
        ("tab", "switch between VMs and images"),
        ("↑↓ j k", "select · g G first / last"),
        ("q", "quit"),
    ],
];

fn draw_help(frame: &mut Frame, area: Rect) {
    let lines = |help: &[(&'static str, &'static str)]| -> Vec<Line<'static>> {
        help.iter()
            .map(|(key, what)| match *key {
                "" => Line::styled(*what, HEADER),
                _ => Line::from(vec![Span::styled(format!("{key:8}"), ACCENT), Span::raw(*what)]),
            })
            .collect()
    };
    let [left, right] = HELP.map(lines);
    if area.width >= 100 {
        let height = left.len().max(right.len()) as u16 + 4;
        let inner = dialog(frame, area, 92, height, "keys", BORDER, 2);
        let [l, _, r] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(2), Constraint::Fill(1)]).areas(inner);
        frame.render_widget(Paragraph::new(left), l);
        frame.render_widget(Paragraph::new(right), r);
    } else {
        let all = [left, vec![Line::default()], right].concat();
        let inner = dialog(frame, area, 50, all.len() as u16 + 4, "keys", BORDER, 2);
        frame.render_widget(Paragraph::new(all), inner);
    }
}

fn draw_confirm(frame: &mut Frame, area: Rect, what: Vec<Span<'static>>, note: &str) {
    draw_ask(frame, area, "delete", ERROR, what, note, &[("y", "delete"), ("n", "cancel")]);
}

/// A question in a box titled `title`, with the keys that answer it.
fn draw_ask(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    border: Style,
    what: Vec<Span<'static>>,
    note: &str,
    keys: &[(&str, &str)],
) {
    let width = (note.chars().count() as u16 + 8).max(46);
    let inner = dialog(frame, area, width, 8, title, border, 2);
    let mut answers = Vec::new();
    for (i, (key, what)) in keys.iter().enumerate() {
        if i > 0 {
            answers.push(Span::styled(" · ", DIM));
        }
        answers.push(Span::styled(key.to_string(), ACCENT));
        answers.push(Span::styled(format!(" {what}"), DIM));
    }
    let lines = vec![Line::from(what), Line::styled(note.to_string(), DIM), Line::default(), Line::from(answers)];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// `2026-10-02 08:50`, in local time.
fn local_time(secs: u64) -> String {
    let t = secs as libc::time_t;
    // SAFETY: localtime_r only writes the struct it's given.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return String::new();
    }
    format!("{}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

#[cfg(test)]
mod tests {
    use super::*;
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
            mounts: vec![],
            ssh: SshSpec { user: "me".into(), port },
            qemu: None,
        };
        Entry { name: name.into(), vm: Ok((spec, state)), disk: Some(3_100_000_000), setting_up: false }
    }

    fn info(name: &str, on_disk: u64, custom: bool, native: bool) -> Info {
        Info {
            name: name.into(),
            title: if custom { "added by you".into() } else { format!("{name} title") },
            size: 300_000_000,
            on_disk,
            downloading: None,
            custom,
            native,
        }
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

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            let _ = app.key(press(KeyCode::Char(c)));
        }
    }

    fn vms() -> Vec<Entry> {
        vec![
            entry("dev", State::Running, "debian-13", 2222),
            entry("web", State::Stopped, "fedora-44", 2223),
            Entry { name: "old".into(), vm: Err("invalid vx.toml".into()), disk: None, setting_up: false },
        ]
    }

    fn home() -> Home {
        Home::at(std::env::temp_dir().join(format!("vx-tui-{}", std::process::id())))
    }

    fn with_vms() -> App {
        let mut app = App::new(&home(), Arch::Aarch64);
        app.update(vms());
        app.update_images(vec![
            info("debian-13", 337_000_000, false, true),
            info("ubuntu-24.04", 0, false, true),
            info("archlinux", 0, false, false),
            info("mine", 1_000_000_000, true, true),
        ]);
        app
    }

    const VM_GOLDEN: &str = "\
╭─ vx ─ VMs · images ────────────────────────────────────── 3 VMs · 1 running ─╮
│ NAME  STATE         IMAGE      ARCH     CPUS  MEMORY  SSH                    │
│ dev   ● running     debian-13  aarch64     4      4G  127.0.0.1:2222         │
│ web   ○ stopped     fedora-44  aarch64     4      4G  127.0.0.1:2223         │
│ old   ✗ broken                                                               │
│                                                                              │
╰─ ⏎ ssh · x stop · p pause · S snaps · n new · tab images · ? keys · q quit ──╯";

    const IMAGE_GOLDEN: &str = "\
╭─ vx ─ VMs · images ────────────────────────── 4 images · 2 on disk · 1.3 GB ─╮
│ IMAGE         DESCRIPTION               SIZE  ON DISK                        │
│ debian-13     debian-13 title        ~300 MB  337 MB      default            │
│ ubuntu-24.04  ubuntu-24.04 title     ~300 MB  -                              │
│ archlinux     archlinux title        ~300 MB  -           x86_64 only        │
│ mine          added by you            300 MB  1.0 GB      custom             │
│                                                                              │
│                                                                              │
╰─ ⏎ new VM · d delete · a add · tab VMs · ? keys · q quit ────────────────────╯";

    #[test]
    fn draws_the_vm_table() {
        let mut app = with_vms();
        assert_eq!(screen(&mut app, 80, 7), VM_GOLDEN);
        let _ = app.key(press(KeyCode::End));
        assert!(screen(&mut app, 80, 7).contains("╰─ old: invalid vx.toml · n new · tab images · ? keys · q quit ─"));
    }

    #[test]
    fn draws_the_image_table() {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Tab));
        assert_eq!(screen(&mut app, 80, 9), IMAGE_GOLDEN);
    }

    #[test]
    fn hints_follow_the_selection() {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Down));
        assert!(screen(&mut app, 80, 7).contains("⏎ ssh · s start · d delete · S snaps · n new"));
        let _ = app.key(press(KeyCode::Tab));
        let _ = app.key(press(KeyCode::Down)); // ubuntu-24.04: not downloaded
        assert!(screen(&mut app, 80, 7).contains("⏎ new VM · p download · a add · tab VMs"));
    }

    #[test]
    fn shows_loading_then_empty() {
        let mut app = App::new(&home(), Arch::Aarch64);
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

    fn job(app: &mut App, code: KeyCode) -> Option<Job> {
        match app.key(press(code)) {
            Action::Job(job) => Some(job),
            _ => None,
        }
    }

    #[test]
    fn vm_actions_fit_the_state() {
        let mut app = with_vms(); // `dev` is running
        assert_eq!(app.key(press(KeyCode::Enter)), Action::Run(Foreground::Ssh("dev".into())));
        assert_eq!(app.key(press(KeyCode::Char('c'))), Action::Run(Foreground::Console("dev".into())));
        assert_eq!(app.key(press(KeyCode::Char('l'))), Action::Run(Foreground::Logs("dev".into())));
        assert_eq!(app.key(press(KeyCode::Char('s'))), Action::None);
        assert_eq!(app.status.as_ref().unwrap().text, "dev is already running");
        let pause = job(&mut app, KeyCode::Char('p')).unwrap();
        assert_eq!((pause.verb, pause.args), (Verb::Pause, vec!["pause".to_string(), "dev".into()]));

        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Down)); // `web` is stopped
        assert_eq!(app.key(press(KeyCode::Char('c'))), Action::None);
        assert!(app.status.as_ref().unwrap().text.contains("press s to start it"));
        assert_eq!(app.key(press(KeyCode::Char('x'))), Action::None);
        assert_eq!(job(&mut app, KeyCode::Char('s')).unwrap().args, ["start", "web"]);
    }

    #[test]
    fn busy_vms_show_a_spinner_and_refuse_more_work() {
        let mut app = with_vms();
        assert_eq!(job(&mut app, KeyCode::Char('X')).unwrap().args, ["stop", "--force", "dev"]);
        assert!(screen(&mut app, 80, 7).contains(" stopping "));
        assert_eq!(app.key(press(KeyCode::Char('x'))), Action::None);
        assert_eq!(app.status.as_ref().unwrap().text, "dev is busy stopping");

        // Done: the message shows at once, the spinner stays until a refresh brings the new state.
        app.busy.get_mut(&Target::Vm("dev".into())).unwrap().done = true;
        app.say("✓ dev stopped".into(), OK);
        assert!(screen(&mut app, 80, 7).starts_with("╭─ vx ─ VMs · images ─ ✓ dev stopped ─"));
        app.update(vec![entry("dev", State::Stopped, "debian-13", 2222)]);
        assert!(app.busy.is_empty());
        assert!(screen(&mut app, 80, 7).contains("○ stopped"));
    }

    #[test]
    fn delete_asks_first() {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Char('d')));
        assert!(matches!(&app.modal, Some(Modal::DeleteVm(name)) if name == "dev"));
        let text = screen(&mut app, 80, 12);
        assert!(text.contains("Delete dev and its disk?"), "{text}");
        assert_eq!(app.key(press(KeyCode::Char('z'))), Action::None); // ignored, still asking
        assert!(app.modal.is_some());
        assert_eq!(app.key(press(KeyCode::Char('n'))), Action::None);
        assert!(app.modal.is_none());
        let _ = app.key(press(KeyCode::Char('d')));
        assert_eq!(job(&mut app, KeyCode::Char('y')).unwrap().args, ["rm", "-y", "dev"]);
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
    fn new_vm_form_creates_in_the_background() {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Char('n')));
        assert!(matches!(app.modal, Some(Modal::New(_))));
        let text = screen(&mut app, 90, 24);
        assert!(text.contains("─ new VM ─") && text.contains("Name") && text.contains("▸ debian-13"), "{text}");

        // Typing goes to the form, not to the dashboard's keys.
        let ctrl_u = KeyEvent { modifiers: KeyModifiers::CONTROL, ..press(KeyCode::Char('u')) };
        let _ = app.key(ctrl_u);
        typed(&mut app, "box");
        let create = job(&mut app, KeyCode::Enter).unwrap();
        assert_eq!(create.target, Target::Vm("box".into()));
        assert_eq!(&create.args[..5], ["new", "box", "--image", "debian-13", "--disk"]);
        assert!(app.modal.is_none());

        // It shows as a row right away, and is selected once it exists.
        assert!(screen(&mut app, 80, 8).contains(" creating "), "{}", screen(&mut app, 80, 8));
        let mut now = vms();
        now.push(entry("box", State::Running, "debian-13", 2224));
        app.update(now);
        assert_eq!(app.selected().unwrap().name, "box");
        assert!(screen(&mut app, 80, 8).contains(" booting "));
    }

    #[test]
    fn new_vm_dialog_fits_80_columns() {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Char('n')));
        let _ = app.key(press(KeyCode::Tab));
        for _ in 0..14 {
            let _ = app.key(press(KeyCode::Down));
        }
        let text = screen(&mut app, 80, 24);
        assert!(text.contains("▸ archlinux            Arch Linux             578 MB  x86_64 only"), "{text}");
    }

    #[test]
    fn escape_closes_the_form_without_quitting() {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Char('n')));
        assert_eq!(app.key(press(KeyCode::Esc)), Action::None);
        assert!(app.modal.is_none());
    }

    #[test]
    fn image_actions_fit_the_image() {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Tab));
        // debian-13 is on disk: no download, but delete asks first.
        assert_eq!(app.key(press(KeyCode::Char('p'))), Action::None);
        assert_eq!(app.status.as_ref().unwrap().text, "debian-13 is already on disk");
        let _ = app.key(press(KeyCode::Char('d')));
        assert!(screen(&mut app, 80, 14).contains("Delete debian-13 (337 MB)?"));
        assert_eq!(job(&mut app, KeyCode::Char('y')).unwrap().args, ["images", "rm", "debian-13"]);

        let _ = app.key(press(KeyCode::Down)); // ubuntu-24.04, not downloaded
        assert_eq!(job(&mut app, KeyCode::Char('p')).unwrap().args, ["images", "pull", "ubuntu-24.04"]);
        assert!(screen(&mut app, 80, 8).contains(" ↓ 0%"));
        assert_eq!(app.key(press(KeyCode::Char('d'))), Action::None);
        assert_eq!(app.status.as_ref().unwrap().text, "ubuntu-24.04 is busy downloading");

        let _ = app.key(press(KeyCode::Down)); // archlinux, no aarch64 build
        assert_eq!(app.key(press(KeyCode::Char('p'))), Action::None);
        assert_eq!(app.status.as_ref().unwrap().text, "archlinux has no aarch64 build");

        // ⏎ opens the new-VM form with this image chosen.
        let _ = app.key(press(KeyCode::Up));
        let _ = app.key(press(KeyCode::Up));
        let _ = app.key(press(KeyCode::Enter));
        let text = screen(&mut app, 90, 24);
        assert!(text.contains("▸ debian-13"), "{text}");
    }

    #[test]
    fn download_progress_shows_in_both_views() {
        let mut app = with_vms();
        let mut images = app.images.clone_for_test();
        images[1].downloading = Some(150_000_000);
        app.update_images(images);
        let _ = app.key(press(KeyCode::Tab));
        assert!(screen(&mut app, 80, 8).contains("↓ 50%"));
    }

    #[test]
    fn add_image_form() {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Tab));
        let _ = app.key(press(KeyCode::Char('a')));
        assert!(screen(&mut app, 80, 14).contains("─ add image ─"));
        let file = std::env::temp_dir().join(format!("vx-tui-{} My Disk.qcow2", std::process::id()));
        std::fs::write(&file, b"").unwrap();
        typed(&mut app, &file.display().to_string());
        let add = job(&mut app, KeyCode::Enter);
        std::fs::remove_file(&file).unwrap();
        let add = add.unwrap();
        let name = format!("vx-tui-{}-my-disk", std::process::id());
        assert_eq!(add.args, ["images".to_string(), "add".into(), name.clone(), file.display().to_string()]);
        assert!(screen(&mut app, 80, 9).contains(" copying"));
    }

    #[test]
    fn help_opens_and_any_key_closes_it() {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Char('?')));
        let text = screen(&mut app, 110, 24);
        for want in ["─ keys ─", "pause or resume", "download it now", "save a snapshot now", "go back to it", "quit"]
        {
            assert!(text.contains(want), "missing {want}:\n{text}");
        }
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
        let many: Vec<Entry> =
            (0..12).map(|i| entry(&format!("vm-{i:02}"), State::Stopped, "debian-13", 2222 + i)).collect();
        let mut app = App::new(&home(), Arch::Aarch64);
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

    /// The `i`th sample, two seconds after the one before, busier each time.
    fn sample(i: u64) -> Sample {
        let busy = i * i * 5;
        Sample {
            cpus: vec![
                (busy * 4, i * 800),
                (busy, i * 200),
                (busy / 2, i * 200),
                (busy * 2 / 3, i * 200),
                (busy / 3, i * 200),
            ],
            mem_total: 4_000_000_000,
            mem_available: 2_800_000_000,
            swap_total: 0,
            swap_free: 0,
            disk_used: 4_200_000_000,
            disk_size: 19_000_000_000,
            load: [0.42, 0.31, 0.2],
            uptime: i as f64 * 2.0,
            net_rx: i * i * 4000,
            net_tx: i * i * 1000,
        }
    }

    #[test]
    fn details_pane_shows_live_numbers_when_wide() {
        let mut app = with_vms();
        assert!(!screen(&mut app, 80, 7).contains("─ dev ─"), "no room at 80 columns");
        let text = screen(&mut app, 130, 30);
        assert!(text.contains("─ dev ─") && text.contains("connecting…"), "{text}");
        assert_eq!(app.watch(), Some("dev"));

        let mut stats = Stats::default();
        for i in 1..20 {
            stats.push(sample(i));
        }
        app.stats.insert("dev".into(), stats);
        let text = screen(&mut app, 130, 30);
        for want in ["● running · up 38s", "3.1 GB on host", "CPU  ", "92%", "c3", "MEM", "DISK", "NET ↓", "74 kB/s"]
        {
            assert!(text.contains(want), "missing {want}:\n{text}");
        }
        assert!(text.contains("1.2 GB / 4.0 GB") && text.contains("4.2 GB / 19.0 GB"), "{text}");

        // Stopped VMs aren't sampled; `i` hides the pane.
        let _ = app.key(press(KeyCode::Down));
        let text = screen(&mut app, 130, 30);
        assert!(text.contains("live numbers show while it runs"), "{text}");
        assert_eq!(app.watch(), None);
        let _ = app.key(press(KeyCode::Up));
        let _ = app.key(press(KeyCode::Char('i')));
        assert!(!screen(&mut app, 130, 30).contains("─ dev ─"));
        assert_eq!(app.watch(), None);
    }

    /// fresh → deps → { try-nix → nix-2, k8s }, at k8s.
    fn snap_history() -> History {
        use crate::backend::Snap;
        let snap = |name: &str, memory: u64| Snap { name: name.into(), created: 1, memory };
        let mut h = History::default();
        h.add(snap("fresh", 0), "first boot".into());
        h.add(snap("deps", 529_000_000), String::new());
        h.add(snap("try-nix", 0), String::new());
        h.add(snap("nix-2", 0), String::new());
        h.current = Some("deps".into());
        h.add(snap("k8s", 0), "kind up".into());
        h
    }

    fn snap_view() -> App {
        let mut app = with_vms();
        let _ = app.key(press(KeyCode::Char('S')));
        assert_eq!(app.view, View::Snapshots);
        assert_eq!(app.focus().as_deref(), Some("dev"));
        app.update_history(Some(("dev".into(), Ok(snap_history()))));
        app
    }

    #[test]
    fn snapshots_are_a_list_of_save_slots() {
        let mut app = snap_view();
        let text = screen(&mut app, 100, 12);
        assert!(text.contains("vx ─ VMs › dev › snapshots"), "{text}");
        assert!(text.contains("5 snapshots · at k8s"), "{text}");
        let rows: Vec<String> =
            text.lines().skip(2).take(5).map(|l| l.split_whitespace().take(2).collect::<Vec<_>>().join(" ")).collect();
        assert_eq!(rows, ["│ ○", "│ ●", "│ ○", "│ ○", "│ ○"], "one marker per row, no graph:\n{text}");
        for want in ["○  fresh ", "●  deps ", "○  try-nix ", "○  nix-2 ", "○  k8s "] {
            assert!(text.contains(want), "missing {want:?}:\n{text}");
        }
        let lines: Vec<&str> = text.lines().collect();
        let legend =
            lines.iter().position(|l| l.contains("● resumes running   ○ boots from disk    name  current snapshot"));
        assert_eq!(legend, Some(lines.len() - 3), "the legend, then a blank line above the keys:\n{text}");
        assert_eq!(lines[lines.len() - 2].trim_matches(['│', ' ']), "", "{text}");
        // The colors carry the meaning: green and blue markers, and a solid label for k8s.
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let find = |text: &str| {
            let (y, line) = (0..12)
                .map(|y| (y, (0..100).map(|x| buffer[(x, y)].symbol()).collect::<String>()))
                .find(|(_, l)| l.contains(text))
                .unwrap();
            (line[..line.find(text).unwrap()].chars().count(), y)
        };
        let (x, y) = find("●  deps");
        assert_eq!(buffer[(x as u16, y)].fg, Color::Green);
        let (x, y) = find("○  fresh");
        assert_eq!(buffer[(x as u16, y)].fg, Color::LightBlue);
        let (x, y) = find("○  k8s ");
        assert_eq!(buffer[(x as u16 + 3, y)].bg, Color::Cyan, "the k of k8s");
        assert_eq!(buffer[(x as u16 + 3, y - 1)].bg, Color::Reset, "nix-2 isn't labelled");
        // Only k8s says where it came from: it isn't the row above.
        assert!(text.contains("from deps · kind up"), "{text}");
        let notes: usize = ["fresh", "deps", "try-nix", "nix-2", "k8s"]
            .iter()
            .map(|n| text.matches(&format!("from {n}")).count())
            .sum();
        assert_eq!(notes, 1, "{text}");

        // k8s is the current snapshot, so that's selected to start with.
        assert_eq!(app.snap_pick().unwrap().snap.name, "k8s");
        let _ = app.key(press(KeyCode::Up));
        assert_eq!(app.snap_pick().unwrap().snap.name, "nix-2");
        let _ = app.key(press(KeyCode::Char('g')));
        assert_eq!(app.snap_pick().unwrap().snap.name, "fresh");

        // Esc goes back to the VMs rather than quitting.
        assert_eq!(app.key(press(KeyCode::Esc)), Action::None);
        assert_eq!(app.view, View::Vms);
    }

    #[test]
    fn snapshot_actions_run_the_cli() {
        let mut app = snap_view();
        let _ = app.key(press(KeyCode::Up)); // nix-2
        let _ = app.key(press(KeyCode::Enter));
        let text = screen(&mut app, 100, 16);
        assert!(text.contains("Take dev back to nix-2?") && text.contains("Other snapshots are kept."), "{text}");
        let back = job(&mut app, KeyCode::Char('s')).unwrap();
        assert_eq!(back.args, ["snap", "dev"], "saves first");
        assert_eq!(back.then.unwrap(), ["snap", "restore", "-y", "dev", "nix-2"]);
        assert!(screen(&mut app, 100, 16).contains(" restoring"), "on the current snapshot's row");
        // Busy until it's done.
        let _ = app.key(press(KeyCode::Char('c')));
        assert!(app.modal.is_none());
        assert_eq!(app.status.as_ref().unwrap().text, "dev is busy restoring");

        let mut app = snap_view();
        let _ = app.key(press(KeyCode::Char('g'))); // fresh
        let _ = app.key(press(KeyCode::Char('d')));
        assert!(screen(&mut app, 100, 16).contains("its other snapshots aren't touched"));
        assert_eq!(job(&mut app, KeyCode::Char('y')).unwrap().args, ["snap", "rm", "-y", "dev", "fresh"]);

        let mut app = snap_view();
        let _ = app.key(press(KeyCode::Char('c')));
        assert!(screen(&mut app, 100, 16).contains("─ snapshot ─"));
        typed(&mut app, "x");
        let _ = app.key(press(KeyCode::Tab));
        typed(&mut app, "before upgrade");
        let save = job(&mut app, KeyCode::Enter).unwrap();
        assert_eq!(save.args, ["snap", "dev", "snap-1x", "-m", "before upgrade"]);
    }

    #[test]
    fn ctrl_s_saves_right_away() {
        let ctrl_s = KeyEvent { modifiers: KeyModifiers::CONTROL, ..press(KeyCode::Char('s')) };
        let mut app = with_vms();
        let _ = screen(&mut app, 130, 20); // the details pane, so its history loads
        app.update_history(Some(("dev".into(), Ok(snap_history()))));
        let save = match app.key(ctrl_s) {
            Action::Job(job) => job,
            other => panic!("{other:?}"),
        };
        assert_eq!(save.args, ["snap", "dev", "snap-1"]);
        assert_eq!(save.done.as_deref(), Some("✓ saved dev as snap-1"));
    }

    #[test]
    fn snapshot_details_and_vm_pane_section() {
        let mut app = snap_view();
        let _ = app.key(press(KeyCode::Char('g'))); // fresh
        let _ = app.key(press(KeyCode::Down)); // deps
        let text = screen(&mut app, 130, 20);
        assert!(text.contains("─ deps ─") && text.contains("not the current snapshot"), "{text}");
        assert!(text.contains("memory and disk · 529 MB of memory") && text.contains("from   fresh"), "{text}");
        let _ = app.key(press(KeyCode::Char('G'))); // k8s, where it is
        assert!(screen(&mut app, 130, 20).contains("current snapshot · ● running"));

        let _ = app.key(press(KeyCode::Esc));
        let text = screen(&mut app, 130, 30);
        assert!(text.contains("SNAPSHOTS") && text.contains("○  k8s ") && text.contains("○  nix-2 "), "{text}");
        assert!(!text.contains(" deps "), "only the newest three:\n{text}");
        assert!(text.contains("S all (2 more) · ctrl-s save one"), "{text}");
    }

    #[test]
    fn details_show_mounts() {
        let mut app = with_vms();
        assert!(!screen(&mut app, 130, 20).contains("mount "), "nothing mounted, so no line for it");
        if let Some(Entry { vm: Ok((spec, _)), .. }) = app.vms.as_mut().and_then(|v| v.first_mut()) {
            spec.mounts =
                vec![crate::vx::Mount { host: "/Users/me/code".into(), guest: "~/code".into(), read_only: false }];
        }
        assert!(screen(&mut app, 130, 20).contains("mount ~/code"));
    }

    #[test]
    fn ports_dialog_adds_and_removes() {
        let mut app = with_vms(); // dev: SSH on 2222, nothing forwarded
        assert!(screen(&mut app, 130, 20).contains("ports none · f to forward one"));
        let _ = app.key(press(KeyCode::Char('f')));
        let text = screen(&mut app, 100, 20);
        assert!(text.contains("─ dev's ports ─") && text.contains("localhost:2222  → dev:22  ssh"), "{text}");
        // Nothing to pick, so it starts on adding one, and checks what's typed.
        typed(&mut app, "2222");
        assert!(screen(&mut app, 100, 20).contains("2222 is dev's SSH port"));
        let _ = app.key(press(KeyCode::Backspace));
        let _ = app.key(press(KeyCode::Backspace));
        typed(&mut app, "abc:80"); // letters are ignored
        let add = job(&mut app, KeyCode::Enter).unwrap();
        assert_eq!(add.args, ["port", "dev", "22:80"]);
        assert_eq!(add.done.as_deref(), Some("✓ localhost:22 → dev:80"));

        // With forwards to pick, d removes the selected one.
        let mut app = with_vms();
        if let Some(Entry { vm: Ok((spec, _)), .. }) = app.vms.as_mut().and_then(|v| v.first_mut()) {
            spec.forward = vec!["8080:80".into(), "3000:3000".into()];
        }
        assert!(screen(&mut app, 130, 20).contains("ports 8080 → 80, 3000 → 3000 · f to change"));
        let _ = app.key(press(KeyCode::Char('f')));
        let _ = app.key(press(KeyCode::Down));
        assert!(screen(&mut app, 100, 20).contains("▸ localhost:3000  → dev:3000"));
        let rm = job(&mut app, KeyCode::Char('d')).unwrap();
        assert_eq!(rm.args, ["port", "rm", "dev", "3000"]);

        let mut app = with_vms();
        if let Some(Entry { vm: Ok((spec, _)), .. }) = app.vms.as_mut().and_then(|v| v.first_mut()) {
            spec.forward = vec!["8080:80".into()];
        }
        let _ = app.key(press(KeyCode::Char('f')));
        let _ = app.key(press(KeyCode::Char('a')));
        typed(&mut app, "8080:81");
        assert!(screen(&mut app, 100, 20).contains("8080 is already forwarded"));
        assert_eq!(app.key(press(KeyCode::Esc)), Action::None, "back to the list");
        assert_eq!(app.key(press(KeyCode::Esc)), Action::None, "closed");
        assert!(app.modal.is_none());
    }

    #[test]
    fn graphs_and_meters() {
        let history: VecDeque<f64> = [0.0, 0.25, 0.5, 1.0].into();
        let rows: Vec<String> = graph(&history, 1.0, 6, 2, heat).iter().map(|l| l.to_string()).collect();
        assert_eq!(rows, ["     █", "   ▄██"]);
        let cells: String = meter(0.5, 8).iter().map(|s| s.content.to_string()).collect();
        assert_eq!(cells, "■■■■■■■■");
        assert_eq!(meter(0.5, 8).len(), 2, "one span for the filled cells, one for the rest");
        assert_eq!(amount(0), "0 B");
        assert_eq!(amount(1500), "1 kB");
        assert_eq!(uptime(42.0), "42s");
        assert_eq!(uptime(7980.0), "2h 13m");
        assert_eq!(uptime(90000.0), "1d 1h");
    }

    #[test]
    fn long_names_are_clipped() {
        assert_eq!(clip("debian-13", IMAGE_MAX), "debian-13");
        assert_eq!(clip("debian-13-aarch64-d8470b8c6c38", IMAGE_MAX), "debian-13-aarch64…");
    }

    trait CloneForTest {
        fn clone_for_test(&self) -> Vec<Info>;
    }

    impl CloneForTest for Vec<Info> {
        fn clone_for_test(&self) -> Vec<Info> {
            self.iter()
                .map(|i| Info {
                    name: i.name.clone(),
                    title: i.title.clone(),
                    size: i.size,
                    on_disk: i.on_disk,
                    downloading: i.downloading,
                    custom: i.custom,
                    native: i.native,
                })
                .collect()
        }
    }
}
