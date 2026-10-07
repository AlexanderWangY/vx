//! Interactive forms drawn inline in the terminal, used when a command is missing arguments:
//! `vx new` without a name opens a form, and `vx ssh` without a VM opens a picker.
//!
//! Each form only fills in the same arguments the flags do, then the normal command runs.

use std::io::{self, IsTerminal, Stdout};

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

use crate::backend::{self, State};
use crate::host::Arch;
use crate::image::{self, Source};
use crate::ports;
use crate::progress::bytes;
use crate::set;
use crate::setup;
use crate::snapshot;
use crate::style::{self, OUT};
use crate::vx::{self, Home, Spec};
use crate::{NewArgs, hinted};

const ACCENT: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const ERROR: Style = Style::new().fg(Color::Red);
const OK: Style = Style::new().fg(Color::Green);
const WARN: Style = Style::new().fg(Color::Yellow);

/// Forms need someone at the keyboard and a screen to draw on.
pub fn interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// What a key press did to a form.
pub enum Step<T> {
    Continue,
    Cancel,
    Done(T),
}

trait Form {
    type Output;
    fn draw(&mut self, frame: &mut Frame);
    fn key(&mut self, key: KeyEvent) -> Step<Self::Output>;
}

/// Run a form in an inline viewport `height` rows tall, until it's done or cancelled.
fn run<F: Form>(height: u16, form: &mut F) -> Result<Option<F::Output>> {
    let mut inline = Inline::open(height)?;
    loop {
        inline.terminal.draw(|frame| {
            form.draw(frame);
            // NO_COLOR keeps bold and dim, which carry the focus, but drops colors.
            if !OUT.enabled() {
                style::strip_colors(frame.buffer_mut());
            }
        })?;
        // Anything but a key press (a resize, a focus change) just redraws.
        let Event::Key(press) = event::read()? else { continue };
        if press.kind == KeyEventKind::Release {
            continue;
        }
        if press.code == KeyCode::Char('c') && press.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(None);
        }
        match form.key(press) {
            Step::Continue => {}
            Step::Cancel => return Ok(None),
            Step::Done(value) => return Ok(Some(value)),
        }
    }
}

/// Raw mode plus an inline viewport. Dropping it, even while unwinding from a panic,
/// erases the form and leaves the cursor where the form began, so the shell is left clean.
struct Inline {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Inline {
    fn open(height: u16) -> Result<Inline> {
        let rows = terminal::size()?.1;
        terminal::enable_raw_mode()?;
        let options = TerminalOptions { viewport: Viewport::Inline(height.min(rows)) };
        match Terminal::with_options(CrosstermBackend::new(io::stdout()), options) {
            Ok(terminal) => Ok(Inline { terminal }),
            Err(e) => {
                let _ = terminal::disable_raw_mode();
                // Drawing in place needs the cursor position, which some multiplexers
                // and editor shells never report.
                Err(hinted(
                    format!("can't draw the form in this terminal ({e})"),
                    "pass the arguments instead, e.g. `vx new <name>`",
                ))
            }
        }
    }
}

impl Drop for Inline {
    fn drop(&mut self) {
        let top = self.terminal.get_frame().area().as_position();
        let _ = self.terminal.clear();
        let _ = execute!(io::stdout(), cursor::MoveTo(top.x, top.y), cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

/// A one-line text field.
#[derive(Default)]
struct Input {
    text: String,
    /// In characters.
    cursor: usize,
}

impl Input {
    fn new(text: &str) -> Input {
        Input { text: text.into(), cursor: text.chars().count() }
    }

    fn byte(&self, char_index: usize) -> usize {
        self.text.char_indices().nth(char_index).map_or(self.text.len(), |(i, _)| i)
    }

    /// Edit with `key`. `accept` maps a typed character to what's inserted, or rejects it.
    fn edit(&mut self, key: KeyEvent, max: usize, accept: impl Fn(char) -> Option<char>) {
        let len = self.text.chars().count();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char('u') if ctrl => {
                self.text.drain(..self.byte(self.cursor));
                self.cursor = 0;
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                if let Some(c) = accept(c)
                    && len < max
                {
                    self.text.insert(self.byte(self.cursor), c);
                    self.cursor += 1;
                }
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.text.remove(self.byte(self.cursor));
            }
            KeyCode::Delete if self.cursor < len => {
                self.text.remove(self.byte(self.cursor));
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Image,
    Cpus,
    Memory,
    Disk,
    Install,
    Setup,
}

const FIELDS: [Field; 7] =
    [Field::Name, Field::Image, Field::Cpus, Field::Memory, Field::Disk, Field::Install, Field::Setup];

/// One row of the image list.
struct Choice {
    /// What `--image` gets.
    arg: String,
    title: String,
    size: String,
    cached: bool,
    /// Whether it's built for the VM's architecture; if not, it runs as the other one, emulated.
    native: bool,
    /// Added with `vx images add`.
    custom: bool,
}

/// The `vx new` form. Also shown inside the dashboard.
pub struct NewForm {
    home: Home,
    arch: Arch,
    host_cpus: usize,
    defaults: Spec,
    no_start: bool,
    focus: Field,
    name: Input,
    images: Vec<Choice>,
    selected: usize,
    /// The first image row on screen.
    scroll: usize,
    cpus: Input,
    memory: Input,
    disk: Input,
    /// Packages, separated by commas or spaces; starts as the defaults from config.toml.
    install: Input,
    /// The setup script; starts as the default from config.toml.
    setup: Input,
    /// Whether the two above came from config.toml, for the tips.
    has_defaults: bool,
    /// `--mount` flags, passed through as they are.
    mount: Vec<String>,
}

/// Rows besides the image list: name, blank, blank, sizes, blank, install, setup, blank,
/// message, help.
const FIXED_ROWS: u16 = 10;
const IMAGE_ROWS: u16 = 7;
const LABEL: usize = 8;
const MARGIN: &str = "  ";

/// Fill in `vx new`'s arguments with a form, starting from the ones given as flags.
/// Returns `None` if cancelled.
pub fn new_vm(home: &Home, args: NewArgs) -> Result<Option<NewArgs>> {
    let mut form = NewForm::new(home, args)?;
    run(form.height(), &mut form)
}

impl NewForm {
    pub fn new(home: &Home, args: NewArgs) -> Result<NewForm> {
        let defaults = Spec::defaults()?;
        let arch = args.arch.unwrap_or(defaults.arch);
        let mut images: Vec<Choice> = image::CATALOG
            .iter()
            .map(|i| Choice {
                arg: i.name.into(),
                title: i.title.into(),
                size: format!("{} MB", i.size_mb),
                cached: !i.cached(home, arch).is_empty(),
                native: i.supports(arch),
                custom: false,
            })
            .collect();
        images.extend(image::customs(home).into_iter().map(|c| Choice {
            arg: c.name,
            title: "added by you".into(),
            size: bytes(c.size),
            cached: true,
            native: true,
            custom: true,
        }));
        // `--image ./disk.qcow2` shows up as its own row.
        if let Source::File(path) = Source::parse(home, &args.image)? {
            let size = path.metadata().map(|m| bytes(m.len())).unwrap_or_default();
            let file = Choice {
                arg: args.image.clone(),
                title: "local file".into(),
                size,
                cached: true,
                native: true,
                custom: false,
            };
            images.insert(0, file);
        }
        let selected = images.iter().position(|c| c.arg == args.image).unwrap_or(0);
        let name = args.name.unwrap_or_else(|| suggest_name(home));
        let config = setup::Config::load(home)?;
        let plan = setup::Plan::new(&config.new, args.bare, &args.install, args.setup.as_deref());
        let has_defaults = !args.bare && (!config.new.install.is_empty() || config.new.setup.is_some());
        Ok(NewForm {
            home: Home::at(home.root()),
            arch,
            host_cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
            no_start: args.no_start,
            focus: Field::Name,
            name: Input::new(&name),
            images,
            selected,
            scroll: 0,
            cpus: Input::new(&args.cpus.unwrap_or(defaults.cpus).to_string()),
            memory: Input::new(args.mem.as_deref().unwrap_or(&defaults.memory)),
            disk: Input::new(&args.disk),
            install: Input::new(&plan.install.join(", ")),
            setup: Input::new(plan.setup.as_deref().unwrap_or_default()),
            has_defaults,
            mount: args.mount,
            defaults,
        })
    }

    fn input(&mut self, field: Field) -> Option<&mut Input> {
        match field {
            Field::Name => Some(&mut self.name),
            Field::Cpus => Some(&mut self.cpus),
            Field::Memory => Some(&mut self.memory),
            Field::Disk => Some(&mut self.disk),
            Field::Install => Some(&mut self.install),
            Field::Setup => Some(&mut self.setup),
            Field::Image => None,
        }
    }

    /// The packages typed into Install.
    fn packages(&self) -> Vec<String> {
        self.install.text.split([',', ' ']).filter(|p| !p.is_empty()).map(String::from).collect()
    }

    /// Why `field` can't be used as is.
    fn problem(&self, field: Field) -> Option<String> {
        match field {
            Field::Name if self.name.text.is_empty() => Some("give it a name".into()),
            Field::Name if vx::validate_name(&self.name.text).is_err() => {
                Some("use lowercase letters, digits and -, starting with a letter".into())
            }
            Field::Name => self.home.check_new_name(&self.name.text).err().map(|e| e.to_string()),
            Field::Image => None,
            Field::Cpus => match self.cpus.text.parse::<u32>() {
                Ok(n) if n >= 1 => None,
                _ => Some("at least 1".into()),
            },
            Field::Memory => (!vx::is_size(&self.memory.text)).then(|| "use a size like 4G or 512M".into()),
            Field::Disk => (!vx::is_size(&self.disk.text)).then(|| "use a size like 20G or 50G".into()),
            Field::Install => self.packages().iter().find_map(|p| setup::check_package(p).err()).map(|e| e.to_string()),
            Field::Setup if self.setup.text.trim().is_empty() => None,
            Field::Setup => {
                let path = setup::expand(self.setup.text.trim());
                (!path.is_file()).then(|| format!("no file at {}", path.display()))
            }
        }
    }

    fn move_focus(&mut self, by: isize) {
        let i = FIELDS.iter().position(|f| *f == self.focus).unwrap_or(0) as isize;
        self.focus = FIELDS[(i + by).rem_euclid(FIELDS.len() as isize) as usize];
    }

    fn select(&mut self, index: usize) {
        self.selected = index.min(self.images.len() - 1);
    }

    /// Rows it needs to show the whole image list.
    pub fn height(&self) -> u16 {
        FIXED_ROWS + IMAGE_ROWS.min(self.images.len() as u16)
    }

    pub fn handle(&mut self, key: KeyEvent) -> Step<NewArgs> {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc => return Step::Cancel,
            KeyCode::Enter => return self.submit(),
            KeyCode::Tab if shift => self.move_focus(-1),
            KeyCode::BackTab => self.move_focus(-1),
            KeyCode::Tab => self.move_focus(1),
            KeyCode::Up if self.focus == Field::Image && self.selected > 0 => self.select(self.selected - 1),
            KeyCode::Down if self.focus == Field::Image && self.selected + 1 < self.images.len() => {
                self.select(self.selected + 1)
            }
            // Elsewhere, and past the ends of the list, ↑↓ move between rows.
            KeyCode::Up => {
                self.focus = match self.focus {
                    Field::Name | Field::Image => Field::Name,
                    Field::Cpus | Field::Memory | Field::Disk => Field::Image,
                    Field::Install => Field::Cpus,
                    Field::Setup => Field::Install,
                }
            }
            KeyCode::Down => {
                self.focus = match self.focus {
                    Field::Name => Field::Image,
                    Field::Image => Field::Cpus,
                    Field::Cpus | Field::Memory | Field::Disk => Field::Install,
                    Field::Install | Field::Setup => Field::Setup,
                }
            }
            _ if self.focus == Field::Image => self.image_key(key),
            _ => {
                let field = self.focus;
                let (max, accept): (usize, fn(char) -> Option<char>) = match field {
                    // Typed names are nudged into shape: `Web 1` becomes `web-1`.
                    Field::Name => (32, |c| match c {
                        'a'..='z' | '0'..='9' | '-' => Some(c),
                        'A'..='Z' => Some(c.to_ascii_lowercase()),
                        ' ' | '_' | '.' => Some('-'),
                        _ => None,
                    }),
                    Field::Cpus => (3, |c| c.is_ascii_digit().then_some(c)),
                    Field::Install => (400, |c| {
                        (c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '@' | ':' | ',' | ' '))
                            .then_some(c)
                    }),
                    Field::Setup => (4096, |c| (!c.is_control()).then_some(c)),
                    _ => (8, |c| match c {
                        '0'..='9' => Some(c),
                        'k' | 'm' | 'g' | 't' | 'K' | 'M' | 'G' | 'T' => Some(c.to_ascii_uppercase()),
                        _ => None,
                    }),
                };
                if let Some(input) = self.input(field) {
                    input.edit(key, max, accept);
                }
            }
        }
        Step::Continue
    }

    fn image_key(&mut self, key: KeyEvent) {
        let page = IMAGE_ROWS as usize;
        match key.code {
            KeyCode::PageUp => self.select(self.selected.saturating_sub(page)),
            KeyCode::PageDown => self.select(self.selected + page),
            KeyCode::Home => self.select(0),
            KeyCode::End => self.select(usize::MAX),
            KeyCode::Char('k') => self.select(self.selected.saturating_sub(1)),
            KeyCode::Char('j') => self.select(self.selected + 1),
            // Any other letter jumps to the next image starting with it.
            KeyCode::Char(c) => {
                let c = c.to_ascii_lowercase();
                let n = self.images.len();
                if let Some(i) = (1..=n).map(|d| (self.selected + d) % n).find(|&i| self.images[i].arg.starts_with(c)) {
                    self.select(i);
                }
            }
            _ => {}
        }
    }

    fn submit(&mut self) -> Step<NewArgs> {
        if let Some(field) = FIELDS.into_iter().find(|f| self.problem(*f).is_some()) {
            self.focus = field;
            return Step::Continue;
        }
        let choice = &self.images[self.selected];
        let arch = if choice.native { self.arch } else { other(self.arch) };
        let host = self.defaults.arch;
        Step::Done(NewArgs {
            name: Some(self.name.text.clone()),
            interactive: false,
            image: choice.arg.clone(),
            cpus: Some(self.cpus.text.parse().unwrap_or(self.defaults.cpus)),
            mem: Some(self.memory.text.clone()),
            disk: self.disk.text.clone(),
            arch: (arch != host).then_some(arch),
            no_start: self.no_start,
            // What the form shows is the whole of it, defaults included.
            install: self.packages(),
            setup: Some(self.setup.text.trim().to_string()).filter(|s| !s.is_empty()),
            bare: true,
            mount: self.mount.clone(),
        })
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let rows = area.height.saturating_sub(FIXED_ROWS).max(1) as usize;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }

        let mut lines = Vec::new();
        let mut cursor = None;
        let y = |lines: &Vec<Line>| area.y + lines.len() as u16;

        // Name
        let mark = match self.problem(Field::Name) {
            None => Span::styled("✓", OK),
            Some(_) => Span::styled("✗", ERROR),
        };
        if self.focus == Field::Name {
            cursor = Some(Position::new(area.x + (MARGIN.len() + LABEL + self.name.cursor) as u16, y(&lines)));
        }
        lines.push(Line::from(vec![
            Span::raw(MARGIN),
            self.label("Name", Field::Name),
            Span::styled(format!("{:33}", self.name.text), self.value_style(Field::Name)),
            mark,
        ]));
        lines.push(Line::default());

        // Image list, with ↑/↓ in the label column when there's more to scroll to.
        let name_w = self.images.iter().map(|c| c.arg.len()).max().unwrap_or(0).min(24);
        let title_w = self.images.iter().map(|c| c.title.len()).max().unwrap_or(0);
        let end = (self.scroll + rows).min(self.images.len());
        for (row, i) in (self.scroll..end).enumerate() {
            let choice = &self.images[i];
            let label = if row == 0 {
                self.label("Image", Field::Image)
            } else if row == 1 && self.scroll > 0 {
                Span::styled(format!("{:LABEL$}", "  ↑"), DIM)
            } else if row + 1 == rows && end < self.images.len() {
                Span::styled(format!("{:LABEL$}", "  ↓"), DIM)
            } else {
                Span::raw(" ".repeat(LABEL))
            };
            let chosen = i == self.selected;
            let style = match (chosen, self.focus == Field::Image) {
                (true, true) => ACCENT,
                (true, false) => Style::new().add_modifier(Modifier::BOLD),
                _ if !choice.native => DIM,
                _ => Style::new(),
            };
            let note = if !choice.native {
                Span::styled(format!("{} only", other(self.arch)), WARN)
            } else if choice.custom {
                Span::styled("custom", Style::new().fg(Color::Cyan))
            } else if choice.cached {
                Span::styled("cached", OK)
            } else {
                Span::raw("")
            };
            let name: String = choice.arg.chars().take(name_w).collect();
            lines.push(Line::from(vec![
                Span::raw(MARGIN),
                label,
                Span::styled(if chosen { "▸ " } else { "  " }, style),
                Span::styled(format!("{name:name_w$}  {:title_w$}  {:>7}  ", choice.title, choice.size), style),
                note,
            ]));
        }
        lines.push(Line::default());

        // CPUs, memory and disk on one row.
        let mut spans = vec![Span::raw(MARGIN)];
        for (field, title, width) in [(Field::Cpus, "CPUs", 6), (Field::Memory, "Memory", 6), (Field::Disk, "Disk", 6)]
        {
            let input = match field {
                Field::Cpus => &self.cpus,
                Field::Memory => &self.memory,
                _ => &self.disk,
            };
            if self.focus == field {
                let x: usize = spans.iter().map(|s| s.content.chars().count()).sum::<usize>() + LABEL;
                cursor = Some(Position::new(area.x + (x + input.cursor) as u16, y(&lines)));
            }
            spans.push(self.label(title, field));
            spans.push(Span::styled(format!("{:width$}   ", input.text), self.value_style(field)));
        }
        lines.push(Line::from(spans));
        lines.push(Line::default());

        // What to install and run once it's up, each scrolled to keep the cursor in view.
        for (field, title) in [(Field::Install, "Install"), (Field::Setup, "Setup")] {
            let input = if field == Field::Install { &self.install } else { &self.setup };
            let room = (area.width as usize).saturating_sub(MARGIN.len() + LABEL + 1).max(1);
            let skip = input.cursor.saturating_sub(room);
            let shown: String = input.text.chars().skip(skip).take(room).collect();
            if self.focus == field {
                let x = MARGIN.len() + LABEL + input.cursor - skip;
                cursor = Some(Position::new(area.x + x as u16, y(&lines)));
            }
            let shown = if shown.is_empty() && self.focus != field {
                Span::styled("none", DIM)
            } else {
                Span::styled(shown, self.value_style(field))
            };
            lines.push(Line::from(vec![Span::raw(MARGIN), self.label(title, field), shown]));
        }
        lines.push(Line::default());

        lines.push(Line::from(vec![Span::raw(MARGIN), self.message()]));
        let help = if self.focus == Field::Image {
            "↑↓ choose · tab next field · enter create · esc cancel"
        } else {
            "tab/↑↓ move · enter create · esc cancel"
        };
        lines.push(Line::from(vec![Span::raw(MARGIN), Span::styled(help, DIM)]));

        frame.render_widget(Paragraph::new(lines), area);
        if let Some(position) = cursor.filter(|p| area.contains(*p)) {
            frame.set_cursor_position(position);
        }
    }

    fn label(&self, text: &str, field: Field) -> Span<'static> {
        let style = if self.focus == field { ACCENT } else { Style::new() };
        Span::styled(format!("{text:LABEL$}"), style)
    }

    fn value_style(&self, field: Field) -> Style {
        if self.problem(field).is_some() && field != Field::Name { ERROR } else { Style::new() }
    }

    /// A line about the focused field: what's wrong with it, or a tip.
    fn message(&self) -> Span<'static> {
        if let Some(problem) = self.problem(self.focus) {
            return Span::styled(problem, ERROR);
        }
        let tip = match self.focus {
            Field::Name => format!("ssh into it later with `vx ssh {}`", self.name.text),
            Field::Image => {
                let choice = &self.images[self.selected];
                if !choice.native {
                    return Span::styled(
                        format!(
                            "no {} build, so it runs as {} under emulation (much slower)",
                            self.arch,
                            other(self.arch)
                        ),
                        WARN,
                    );
                }
                if choice.cached {
                    "already downloaded".into()
                } else {
                    format!("downloads {} once, then it's cached", choice.size)
                }
            }
            Field::Cpus => {
                let n: usize = self.cpus.text.parse().unwrap_or(0);
                if n > self.host_cpus {
                    return Span::styled(format!("more than this machine's {} cores", self.host_cpus), WARN);
                }
                format!("this machine has {} cores", self.host_cpus)
            }
            Field::Memory => "e.g. 2G, 8G or 512M".into(),
            Field::Disk => "the VM's disk grows to this size".into(),
            Field::Install if self.has_defaults => {
                "your defaults from ~/.vx/config.toml; change them for this VM".into()
            }
            Field::Install => "e.g. git, build-tools, python; those last two work on any distro".into(),
            Field::Setup => "a script of yours, run in the VM as you once the packages are in".into(),
        };
        Span::styled(tip, DIM)
    }
}

impl Form for NewForm {
    type Output = NewArgs;

    fn draw(&mut self, frame: &mut Frame) {
        self.render(frame, frame.area());
    }

    fn key(&mut self, key: KeyEvent) -> Step<NewArgs> {
        self.handle(key)
    }
}

/// The dashboard's "add image" form: a file and the name to add it under.
pub struct AddImageForm {
    home: Home,
    /// 0: file, 1: name.
    focus: usize,
    file: Input,
    name: Input,
    /// Until the name is typed in, it follows the file's name.
    name_edited: bool,
}

/// What the add-image form produces: the name, and the file's absolute path.
pub struct NewImage {
    pub name: String,
    pub file: String,
}

impl AddImageForm {
    pub const HEIGHT: u16 = 6;

    pub fn new(home: &Home) -> AddImageForm {
        AddImageForm {
            home: Home::at(home.root()),
            focus: 0,
            file: Input::default(),
            name: Input::default(),
            name_edited: false,
        }
    }

    /// The file path, with `~` expanded.
    fn path(&self) -> std::path::PathBuf {
        let text = self.file.text.trim();
        match (text.strip_prefix("~/"), std::env::home_dir()) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => std::path::PathBuf::from(text),
        }
    }

    fn problem(&self, field: usize) -> Option<String> {
        if field == 0 {
            let path = self.path();
            return if self.file.text.trim().is_empty() {
                Some("the path to a qcow2 or raw disk image".into())
            } else if path.is_dir() {
                Some("that's a folder; pick a disk image file".into())
            } else if !path.is_file() {
                Some("no such file".into())
            } else {
                None
            };
        }
        if self.name.text.is_empty() {
            return Some("give it a name".into());
        }
        image::check_custom_name(&self.home, &self.name.text).err().map(|e| e.to_string())
    }

    /// `~/Downloads/My Image.qcow2` → `my-image`
    fn name_from_file(&self) -> String {
        let stem = self.path().file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        let name: String =
            stem.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '.') { c } else { '-' }).collect();
        name.trim_matches(|c: char| !c.is_ascii_alphanumeric()).chars().take(32).collect()
    }

    pub fn handle(&mut self, key: KeyEvent) -> Step<NewImage> {
        match key.code {
            KeyCode::Esc => return Step::Cancel,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => self.focus = 1 - self.focus,
            KeyCode::Enter => {
                if let Some(field) = (0..2).find(|f| self.problem(*f).is_some()) {
                    self.focus = field;
                    return Step::Continue;
                }
                let file = std::path::absolute(self.path()).unwrap_or_else(|_| self.path());
                return Step::Done(NewImage { name: self.name.text.clone(), file: file.display().to_string() });
            }
            _ if self.focus == 0 => {
                self.file.edit(key, 4096, |c| (!c.is_control()).then_some(c));
                if !self.name_edited {
                    self.name = Input::new(&self.name_from_file());
                }
            }
            _ => {
                let before = self.name.text.clone();
                self.name.edit(key, 32, |c| match c {
                    'a'..='z' | '0'..='9' | '-' | '.' => Some(c),
                    'A'..='Z' => Some(c.to_ascii_lowercase()),
                    ' ' | '_' => Some('-'),
                    _ => None,
                });
                self.name_edited |= self.name.text != before;
            }
        }
        Step::Continue
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        let mut cursor = None;
        for (i, (title, input)) in [("File", &self.file), ("Name", &self.name)].into_iter().enumerate() {
            let label_style = if self.focus == i { ACCENT } else { Style::new() };
            let mark = if self.problem(i).is_none() { Span::styled(" ✓", OK) } else { Span::raw("") };
            // Long paths scroll so the cursor stays in view.
            let room = (area.width as usize).saturating_sub(LABEL + 3).max(1);
            let skip = input.cursor.saturating_sub(room);
            let shown: String = input.text.chars().skip(skip).take(room).collect();
            if self.focus == i {
                let x = area.x + (LABEL + input.cursor - skip) as u16;
                cursor = Some(Position::new(x, area.y + lines.len() as u16));
            }
            lines.push(Line::from(vec![Span::styled(format!("{title:LABEL$}"), label_style), Span::raw(shown), mark]));
        }
        lines.push(Line::default());
        lines.push(match self.problem(self.focus) {
            Some(problem) if self.focus == 0 && self.file.text.trim().is_empty() => Line::styled(problem, DIM),
            Some(problem) => Line::styled(problem, ERROR),
            None if self.focus == 0 => Line::styled("it's copied into vx, so the original can move", DIM),
            None => Line::styled(format!("use it with `vx new <name> --image {}`", self.name.text), DIM),
        });
        lines.push(Line::styled("tab move · enter add · esc cancel", DIM));
        frame.render_widget(Paragraph::new(lines), area);
        if let Some(position) = cursor.filter(|p| area.contains(*p)) {
            frame.set_cursor_position(position);
        }
    }
}

/// The dashboard's "ports" dialog: a VM's forwards, to remove one or add another.
pub struct PortsForm {
    vm: String,
    ssh: u16,
    forwards: Vec<(u16, u16)>,
    selected: usize,
    /// Typing a new forward; `None` while picking from the list.
    adding: Option<Input>,
}

/// What the ports dialog asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum PortChange {
    Add(u16, u16),
    Remove(u16),
}

impl PortsForm {
    pub fn new(vm: &str, ssh: u16, forwards: Vec<(u16, u16)>) -> PortsForm {
        // With nothing to pick, start on adding one.
        let adding = forwards.is_empty().then(Input::default);
        PortsForm { vm: vm.into(), ssh, forwards, selected: 0, adding }
    }

    pub fn height(&self) -> u16 {
        self.forwards.len() as u16 + 6
    }

    /// The forward being typed, or why it won't do.
    fn typed(&self) -> Option<Result<(u16, u16), String>> {
        let text = self.adding.as_ref()?.text.trim();
        if text.is_empty() {
            return Some(Err("<port here>:<port in the VM>, e.g. 8080:80, or one port for both".into()));
        }
        let parsed = ports::parse(text).map_err(|_| "write it as 8080:80, or one port like 3000".to_string());
        Some(parsed.and_then(|(host, guest)| {
            if host == self.ssh {
                Err(format!("{host} is {}'s SSH port", self.vm))
            } else if self.forwards.iter().any(|(h, _)| *h == host) {
                Err(format!("{host} is already forwarded"))
            } else {
                Ok((host, guest))
            }
        }))
    }

    pub fn handle(&mut self, key: KeyEvent) -> Step<PortChange> {
        if let Some(input) = &mut self.adding {
            match key.code {
                KeyCode::Esc if self.forwards.is_empty() => return Step::Cancel,
                KeyCode::Esc => self.adding = None,
                KeyCode::Enter => {
                    if let Some(Ok((host, guest))) = self.typed() {
                        return Step::Done(PortChange::Add(host, guest));
                    }
                }
                _ => input.edit(key, 11, |c| (c.is_ascii_digit() || c == ':').then_some(c)),
            }
            return Step::Continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Step::Cancel,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.forwards.len().saturating_sub(1))
            }
            KeyCode::Char('a' | 'n') => self.adding = Some(Input::default()),
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                if let Some((host, _)) = self.forwards.get(self.selected) {
                    return Step::Done(PortChange::Remove(*host));
                }
            }
            _ => {}
        }
        Step::Continue
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![Line::from(vec![
            Span::styled(format!("localhost:{:<6}", self.ssh), DIM),
            Span::styled(format!("→ {}:22  ssh", self.vm), DIM),
        ])];
        for (i, (host, guest)) in self.forwards.iter().enumerate() {
            let picked = self.adding.is_none() && i == self.selected;
            let style = if picked { ACCENT } else { Style::new() };
            let mark = if picked { "▸ " } else { "  " };
            let text = format!("localhost:{host:<6}→ {}:{guest}", self.vm);
            lines.push(Line::from(vec![Span::styled(mark, ACCENT), Span::styled(text, style)]));
        }
        lines.push(Line::default());
        let mut cursor = None;
        match (&self.adding, self.typed()) {
            (Some(input), typed) => {
                let mark = if matches!(typed, Some(Ok(_))) { Span::styled(" ✓", OK) } else { Span::raw("") };
                lines.push(Line::from(vec![
                    Span::styled(format!("{:LABEL$}", "Forward"), ACCENT),
                    Span::raw(input.text.clone()),
                    mark,
                ]));
                cursor = Some(Position::new(area.x + (LABEL + input.cursor) as u16, area.y + lines.len() as u16 - 1));
                lines.push(match typed {
                    Some(Err(why)) if input.text.trim().is_empty() => Line::styled(why, DIM),
                    Some(Err(why)) => Line::styled(why, ERROR),
                    _ => Line::styled("a running VM picks it up straight away; it's saved for next time too", DIM),
                });
                let back = if self.forwards.is_empty() { "cancel" } else { "back" };
                lines.push(Line::styled(format!("enter add · esc {back}"), DIM));
            }
            (None, _) => {
                lines.push(Line::styled("only this machine can reach them: they listen on 127.0.0.1", DIM));
                lines.push(Line::styled("a add · d remove · ↑↓ select · esc close", DIM));
            }
        }
        frame.render_widget(Paragraph::new(lines), area);
        if let Some(position) = cursor.filter(|p| area.contains(*p)) {
            frame.set_cursor_position(position);
        }
    }
}

/// The dashboard's "snapshot" form: a name, and a note to remember it by.
pub struct SnapForm {
    vm: String,
    /// Names already taken.
    taken: Vec<String>,
    /// 0: name, 1: note.
    focus: usize,
    name: Input,
    note: Input,
}

/// What the snapshot form produces.
pub struct NewSnap {
    pub name: String,
    pub note: String,
}

impl SnapForm {
    pub const HEIGHT: u16 = 6;

    pub fn new(vm: &str, suggested: &str, taken: Vec<String>) -> SnapForm {
        SnapForm { vm: vm.into(), taken, focus: 0, name: Input::new(suggested), note: Input::default() }
    }

    fn problem(&self) -> Option<String> {
        let name = &self.name.text;
        if name.is_empty() {
            return Some("give it a name".into());
        }
        if self.taken.contains(name) {
            return Some(format!("{} already has a snapshot called {name}", self.vm));
        }
        snapshot::validate_name(name).err().map(|e| e.to_string())
    }

    pub fn handle(&mut self, key: KeyEvent) -> Step<NewSnap> {
        match key.code {
            KeyCode::Esc => return Step::Cancel,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => self.focus = 1 - self.focus,
            KeyCode::Enter => {
                if self.problem().is_some() {
                    self.focus = 0;
                    return Step::Continue;
                }
                return Step::Done(NewSnap { name: self.name.text.clone(), note: self.note.text.trim().to_string() });
            }
            _ if self.focus == 0 => self.name.edit(key, 32, |c| match c {
                'a'..='z' | '0'..='9' | '-' | '.' => Some(c),
                'A'..='Z' => Some(c.to_ascii_lowercase()),
                ' ' | '_' => Some('-'),
                _ => None,
            }),
            _ => self.note.edit(key, 200, |c| (!c.is_control()).then_some(c)),
        }
        Step::Continue
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        let mut cursor = None;
        for (i, (title, input)) in [("Name", &self.name), ("Note", &self.note)].into_iter().enumerate() {
            let label_style = if self.focus == i { ACCENT } else { Style::new() };
            let mark = match (i, self.problem()) {
                (0, None) => Span::styled(" ✓", OK),
                _ => Span::raw(""),
            };
            let room = (area.width as usize).saturating_sub(LABEL + 3).max(1);
            let skip = input.cursor.saturating_sub(room);
            let shown: String = input.text.chars().skip(skip).take(room).collect();
            if self.focus == i {
                let x = area.x + (LABEL + input.cursor - skip) as u16;
                cursor = Some(Position::new(x, area.y + lines.len() as u16));
            }
            lines.push(Line::from(vec![Span::styled(format!("{title:LABEL$}"), label_style), Span::raw(shown), mark]));
        }
        lines.push(Line::default());
        lines.push(match self.problem() {
            Some(problem) => Line::styled(problem, ERROR),
            None if self.focus == 1 => Line::styled("optional: what's special about this point", DIM),
            None => Line::styled("a running VM's memory is saved too, so it resumes right where it was", DIM),
        });
        lines.push(Line::styled("tab move · enter save · esc cancel", DIM));
        frame.render_widget(Paragraph::new(lines), area);
        if let Some(position) = cursor.filter(|p| area.contains(*p)) {
            frame.set_cursor_position(position);
        }
    }
}

/// How much this machine can give a VM, which the settings form holds it to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub cpus: u32,
    /// Bytes, when known.
    pub memory: Option<u64>,
}

impl Limits {
    pub fn host() -> Limits {
        Limits { cpus: set::max_cpus(), memory: crate::host::memory() }
    }
}

/// A VM's CPUs, memory and disk (`vx set`).
pub struct SettingsForm {
    vm: String,
    running: bool,
    limits: Limits,
    cpus: u32,
    memory: String,
    disk: u64,
    /// 0: CPUs, 1: memory, 2: disk.
    focus: usize,
    inputs: [Input; 3],
}

impl SettingsForm {
    pub const HEIGHT: u16 = 6;

    /// `disk` is its size now, in bytes, if that's known. Left empty, the disk stays as it is.
    pub fn new(vm: &str, spec: &Spec, running: bool, disk: Option<u64>, limits: Limits) -> SettingsForm {
        let inputs = [
            Input::new(&spec.cpus.to_string()),
            Input::new(&spec.memory),
            Input::new(&disk.map(vx::show_size).unwrap_or_default()),
        ];
        let disk = disk.unwrap_or_default();
        let memory = spec.memory.clone();
        SettingsForm { vm: vm.into(), running, limits, cpus: spec.cpus, memory, disk, focus: 0, inputs }
    }

    fn problem(&self) -> Option<(usize, String)> {
        let [cpus, memory, disk] = &self.inputs;
        let max = self.limits.cpus;
        if !cpus.text.parse::<u32>().is_ok_and(|c| (1..=max).contains(&c)) {
            return Some((0, format!("1 to {max} CPUs, as many as this machine has")));
        }
        match vx::size_bytes(&memory.text) {
            None => return Some((1, "memory like 4G or 512M".into())),
            Some(m) if self.limits.memory.is_some_and(|host| m > host) => {
                return Some((1, "more memory than this machine has".into()));
            }
            Some(_) => {}
        }
        match set::disk_target(self.disk, &disk.text) {
            None if disk.text.is_empty() => None,
            None => Some((2, "disk like 40G, or +20G for that much more".into())),
            Some(d) if d < self.disk => Some((2, format!("disks only grow; it's {} now", vx::show_size(self.disk)))),
            Some(_) => None,
        }
    }

    /// Whether what's typed changes CPUs or memory, which a running VM restarts for.
    fn restarts(&self) -> bool {
        let [cpus, memory, _] = &self.inputs;
        self.running
            && (cpus.text != self.cpus.to_string() || vx::size_bytes(&memory.text) != vx::size_bytes(&self.memory))
    }

    /// The `vx set` arguments for what changed; just `set <vm>` when nothing did.
    pub fn handle(&mut self, key: KeyEvent) -> Step<Vec<String>> {
        match key.code {
            KeyCode::Esc => return Step::Cancel,
            KeyCode::Tab | KeyCode::Down => self.focus = (self.focus + 1) % 3,
            KeyCode::BackTab | KeyCode::Up => self.focus = (self.focus + 2) % 3,
            KeyCode::Enter => match self.problem() {
                Some((field, _)) => self.focus = field,
                None => return Step::Done(self.args()),
            },
            _ => {
                let focus = self.focus;
                self.inputs[focus].edit(key, 8, |c| match c {
                    '0'..='9' => Some(c),
                    'k' | 'm' | 'g' | 't' if focus > 0 => Some(c.to_ascii_uppercase()),
                    'K' | 'M' | 'G' | 'T' | '+' if focus > 0 => Some(c),
                    _ => None,
                });
            }
        }
        Step::Continue
    }

    fn args(&self) -> Vec<String> {
        let [cpus, memory, disk] = &self.inputs;
        let mut args = vec!["set".to_string(), self.vm.clone()];
        if cpus.text != self.cpus.to_string() {
            args.extend(["--cpus".into(), cpus.text.clone()]);
        }
        if vx::size_bytes(&memory.text) != vx::size_bytes(&self.memory) {
            args.extend(["--mem".into(), memory.text.clone()]);
        }
        if !disk.text.is_empty() && set::disk_target(self.disk, &disk.text) != Some(self.disk) {
            args.extend(["--disk".into(), disk.text.clone()]);
        }
        if self.restarts() {
            args.push("--restart".into());
        }
        args
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let problem = self.problem();
        let mut lines = Vec::new();
        let mut cursor = None;
        for (i, title) in ["CPUs", "Memory", "Disk"].into_iter().enumerate() {
            let input = &self.inputs[i];
            let label = if self.focus == i { ACCENT } else { Style::new() };
            if self.focus == i {
                cursor = Some(Position::new(area.x + (LABEL + input.cursor) as u16, area.y + i as u16));
            }
            let mark = match &problem {
                Some((field, _)) if *field == i => Span::styled(" ✗", ERROR),
                _ => Span::styled(" ✓", OK),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{title:LABEL$}"), label),
                Span::raw(input.text.clone()),
                mark,
            ]));
        }
        lines.push(Line::default());
        lines.push(match &problem {
            Some((_, why)) => Line::styled(why.clone(), ERROR),
            None if self.restarts() => {
                Line::styled(format!("{} restarts so its CPUs and memory change", self.vm), WARN)
            }
            None => Line::styled("a disk grows straight away, even while it runs", DIM),
        });
        let enter = if self.restarts() { "enter save and restart" } else { "enter save" };
        lines.push(Line::styled(format!("tab move · {enter} · esc cancel"), DIM));
        frame.render_widget(Paragraph::new(lines), area);
        if let Some(position) = cursor.filter(|p| area.contains(*p)) {
            frame.set_cursor_position(position);
        }
    }
}

/// The name for a copy of a VM, or of one of its snapshots (`vx clone`).
pub struct CloneForm {
    home: Home,
    /// `dev`, or `dev@deps`.
    source: String,
    name: Input,
}

impl CloneForm {
    pub const HEIGHT: u16 = 4;

    pub fn new(home: &Home, vm: &str, snap: Option<&str>) -> CloneForm {
        let source = match snap {
            Some(snap) => format!("{vm}@{snap}"),
            None => vm.to_string(),
        };
        CloneForm { home: Home::at(home.root()), source, name: Input::new(&home.clone_name(vm)) }
    }

    fn problem(&self) -> Option<String> {
        if self.name.text.is_empty() {
            return Some("give it a name".into());
        }
        self.home.check_new_name(&self.name.text).err().map(|e| e.to_string())
    }

    /// The `vx clone` arguments.
    pub fn handle(&mut self, key: KeyEvent) -> Step<Vec<String>> {
        match key.code {
            KeyCode::Esc => return Step::Cancel,
            KeyCode::Enter if self.problem().is_none() => {
                return Step::Done(vec!["clone".into(), self.source.clone(), self.name.text.clone()]);
            }
            KeyCode::Enter => {}
            _ => self.name.edit(key, 32, |c| match c {
                'a'..='z' | '0'..='9' | '-' => Some(c),
                'A'..='Z' => Some(c.to_ascii_lowercase()),
                ' ' | '_' => Some('-'),
                _ => None,
            }),
        }
        Step::Continue
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let room = (area.width as usize).saturating_sub(LABEL + 3).max(1);
        let skip = self.name.cursor.saturating_sub(room);
        let shown: String = self.name.text.chars().skip(skip).take(room).collect();
        let mark = if self.problem().is_none() { Span::styled(" ✓", OK) } else { Span::raw("") };
        let lines = vec![
            Line::from(vec![Span::styled(format!("{:LABEL$}", "Name"), ACCENT), Span::raw(shown), mark]),
            Line::default(),
            match self.problem() {
                Some(problem) => Line::styled(problem, ERROR),
                None => {
                    Line::styled(format!("a new VM with a copy of {}'s disk, and a name of its own", self.source), DIM)
                }
            },
            Line::styled("enter clone · esc cancel", DIM),
        ];
        frame.render_widget(Paragraph::new(lines), area);
        let cursor = Position::new(area.x + (LABEL + self.name.cursor - skip) as u16, area.y);
        if area.contains(cursor) {
            frame.set_cursor_position(cursor);
        }
    }
}

fn other(arch: Arch) -> Arch {
    match arch {
        Arch::Aarch64 => Arch::X86_64,
        Arch::X86_64 => Arch::Aarch64,
    }
}

/// `dev`, or `dev-2`, `dev-3`… if that's taken.
fn suggest_name(home: &Home) -> String {
    std::iter::once("dev".to_string())
        .chain((2..100).map(|n| format!("dev-{n}")))
        .find(|name| home.check_new_name(name).is_ok())
        .unwrap_or_default()
}

/// The command line that does what the form did, so it can be typed directly next time.
pub fn command_line(home: &Home, args: &NewArgs) -> Result<String> {
    let defaults = Spec::defaults()?;
    let config = setup::Config::load(home)?;
    let mut line = format!("vx new {}", args.name.as_deref().unwrap_or_default());
    if args.image != image::DEFAULT {
        line += &format!(" --image {}", args.image);
    }
    if args.cpus.is_some_and(|c| c != defaults.cpus) {
        line += &format!(" --cpus {}", args.cpus.unwrap_or_default());
    }
    if let Some(mem) = args.mem.as_deref().filter(|m| *m != defaults.memory) {
        line += &format!(" --mem {mem}");
    }
    if args.disk != "20G" {
        line += &format!(" --disk {}", args.disk);
    }
    if let Some(arch) = args.arch.filter(|a| *a != defaults.arch) {
        line += &format!(" --arch {arch}");
    }
    if args.no_start {
        line += " --no-start";
    }
    for m in &args.mount {
        line += &format!(" --mount {m}");
    }
    // Only what differs from the defaults in config.toml.
    let usual = setup::Plan::new(&config.new, false, &[], None);
    let plan = setup::Plan::new(&config.new, args.bare, &args.install, args.setup.as_deref());
    if plan != usual {
        let extra = plan.install.starts_with(&usual.install) && plan.setup == usual.setup;
        let install = if extra { &plan.install[usual.install.len()..] } else { &plan.install[..] };
        if !extra {
            line += " --bare";
        }
        if !install.is_empty() {
            line += &format!(" --install {}", install.join(","));
        }
        if let (false, Some(script)) = (extra, &plan.setup) {
            line += &format!(" --setup {script}");
        }
    }
    Ok(line)
}

/// One row of the VM picker.
struct Row {
    name: String,
    state: Option<State>,
    detail: String,
}

struct Picker {
    question: String,
    verb: String,
    rows: Vec<Row>,
    selected: usize,
    scroll: usize,
}

/// Rows on screen at most.
const PICKER_ROWS: usize = 8;

/// Pick a VM for `vx <verb>`, starting on the first one in the `prefer` state.
/// Returns `None` if cancelled.
pub fn pick_vm(home: &Home, verb: &str, question: &str, prefer: Option<State>) -> Result<Option<String>> {
    let names = home.names()?;
    if names.is_empty() {
        return Err(hinted("no VMs yet", "create one with `vx new`"));
    }
    let rows: Vec<Row> = names
        .into_iter()
        .map(|name| match home.load(&name) {
            Ok(vm) => {
                let state = backend::get(&vm.spec.backend).ok().map(|b| b.state(&vm));
                let s = &vm.spec;
                Row { name, state, detail: format!("{:7}  {} CPUs  {}", s.arch, s.cpus, s.memory) }
            }
            Err(_) => Row { name, state: None, detail: "can't read its vx.toml".into() },
        })
        .collect();
    let selected = prefer.and_then(|p| rows.iter().position(|r| r.state.as_ref() == Some(&p))).unwrap_or(0);
    let height = rows.len().min(PICKER_ROWS) as u16 + 2;
    let mut picker = Picker { question: question.into(), verb: verb.into(), rows, selected, scroll: 0 };
    run(height, &mut picker)
}

impl Form for Picker {
    type Output = String;

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let visible = (area.height.saturating_sub(2) as usize).max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible {
            self.scroll = self.selected + 1 - visible;
        }
        let name_w = self.rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
        let mut lines = vec![Line::from(vec![Span::raw(MARGIN), Span::styled(self.question.clone(), ACCENT)])];
        for (i, row) in self.rows.iter().enumerate().skip(self.scroll).take(visible) {
            let chosen = i == self.selected;
            let style = if chosen { ACCENT } else { Style::new() };
            let (state, state_style) = match &row.state {
                Some(State::Running) => ("running".to_string(), OK),
                Some(State::Stopped) => ("stopped".to_string(), DIM),
                Some(other) => (other.to_string(), WARN),
                None => ("broken".to_string(), ERROR),
            };
            lines.push(Line::from(vec![
                Span::raw(MARGIN),
                Span::styled(if chosen { "▸ " } else { "  " }, style),
                Span::styled(format!("{:name_w$}  ", row.name), style),
                Span::styled(format!("{state:9}"), state_style),
                Span::styled(row.detail.clone(), DIM),
            ]));
        }
        let position = if self.rows.len() > visible {
            format!(" · {}/{}", self.selected + 1, self.rows.len())
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            Span::raw(MARGIN),
            Span::styled(format!("↑↓ choose · enter {} · esc cancel{position}", self.verb), DIM),
        ]));
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn key(&mut self, key: KeyEvent) -> Step<String> {
        let last = self.rows.len() - 1;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Step::Cancel,
            KeyCode::Enter => return Step::Done(self.rows[self.selected].name.clone()),
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1).min(last),
            KeyCode::Home | KeyCode::PageUp => self.selected = 0,
            KeyCode::End | KeyCode::PageDown => self.selected = last,
            // Any other letter jumps to the next VM starting with it.
            KeyCode::Char(c) => {
                let n = self.rows.len();
                if let Some(i) = (1..=n).map(|d| (self.selected + d) % n).find(|&i| self.rows[i].name.starts_with(c)) {
                    self.selected = i;
                }
            }
            _ => {}
        }
        Step::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyEventState;

    fn home() -> Home {
        Home::at(std::env::temp_dir().join(format!("vx-form-{}", std::process::id())))
    }

    fn args() -> NewArgs {
        NewArgs {
            name: None,
            interactive: true,
            image: image::DEFAULT.into(),
            cpus: None,
            mem: None,
            disk: "20G".into(),
            arch: Some(Arch::Aarch64),
            no_start: false,
            install: vec![],
            setup: None,
            bare: false,
            mount: vec![],
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE }
    }

    fn typed(form: &mut impl Form, text: &str) {
        for c in text.chars() {
            let _ = form.key(press(KeyCode::Char(c)));
        }
    }

    /// The form as text, plus where the cursor is.
    fn screen(form: &mut impl Form, width: u16, height: u16) -> (String, Option<(u16, u16)>) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| form.draw(f)).unwrap();
        let cursor = terminal.get_cursor_position().ok().map(|p| (p.x, p.y));
        let buffer = terminal.backend().buffer();
        let text = (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        (text, cursor)
    }

    fn done<T>(step: Step<T>) -> T {
        match step {
            Step::Done(value) => value,
            Step::Continue => panic!("form isn't done"),
            Step::Cancel => panic!("form was cancelled"),
        }
    }

    fn form(home: &Home) -> NewForm {
        NewForm::new(home, NewArgs { cpus: Some(4), ..args() }).unwrap()
    }

    #[test]
    fn draws_the_new_form() {
        let home = home();
        let mut form = form(&home);
        let (text, cursor) = screen(&mut form, 80, 17);
        assert_eq!(
            text,
            "  Name    dev                              ✓

  Image   ▸ debian-13            Debian 13 (trixie)     337 MB
            debian-12            Debian 12 (bookworm)   340 MB
            ubuntu-26.04         Ubuntu 26.04 LTS       945 MB
            ubuntu-24.04         Ubuntu 24.04 LTS       620 MB
            ubuntu-22.04         Ubuntu 22.04 LTS       705 MB
            fedora-44            Fedora 44 Cloud        528 MB
    ↓       centos-stream-10     CentOS Stream 10       884 MB

  CPUs    4        Memory  4G       Disk    20G

  Install none
  Setup   none

  ssh into it later with `vx ssh dev`
  tab/↑↓ move · enter create · esc cancel"
        );
        assert_eq!(cursor, Some((13, 0)), "cursor after `dev`");
    }

    #[test]
    fn install_and_setup_start_from_the_defaults() {
        // Its own home, so its config.toml can't reach the other tests.
        let home = Home::at(std::env::temp_dir().join(format!("vx-form-config-{}", std::process::id())));
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(setup::Config::path(&home), "[new]\ninstall = [\"git\", \"build-tools\"]\n").unwrap();
        let mut form = form(&home);
        std::fs::remove_dir_all(home.root()).unwrap();
        let (text, _) = screen(&mut form, 80, 17);
        assert!(text.contains("  Install git, build-tools"), "{text}");
        // Tab past the image and sizes to Install; add one, and check what's typed.
        for _ in 0..5 {
            let _ = form.key(press(KeyCode::Tab));
        }
        assert!(screen(&mut form, 80, 17).0.contains("your defaults from ~/.vx/config.toml"));
        for c in ", htop".chars() {
            let _ = form.key(press(KeyCode::Char(c)));
        }
        let _ = form.key(press(KeyCode::Down)); // Setup
        for c in "/no/such/script.sh".chars() {
            let _ = form.key(press(KeyCode::Char(c)));
        }
        assert!(screen(&mut form, 80, 17).0.contains("no file at /no/such/script.sh"));
        assert!(matches!(form.key(press(KeyCode::Enter)), Step::Continue), "a missing script blocks it");
        let _ = form.key(press(KeyCode::Char('u')));
        let ctrl_u = KeyEvent { modifiers: KeyModifiers::CONTROL, ..press(KeyCode::Char('u')) };
        let _ = form.key(ctrl_u);
        let args = done(form.key(press(KeyCode::Enter)));
        assert_eq!(args.install, ["git", "build-tools", "htop"]);
        assert_eq!((args.setup, args.bare), (None, true), "the form's list is the whole of it");
    }

    #[test]
    fn enter_creates_with_defaults() {
        let home = home();
        let args = done(form(&home).key(press(KeyCode::Enter)));
        assert_eq!(args.name.as_deref(), Some("dev"));
        assert_eq!(args.image, "debian-13");
        assert_eq!((args.cpus, args.mem.as_deref(), args.disk.as_str()), (Some(4), Some("4G"), "20G"));
    }

    #[test]
    fn names_are_nudged_into_shape() {
        let home = home();
        let mut form = form(&home);
        let ctrl_u = KeyEvent { modifiers: KeyModifiers::CONTROL, ..press(KeyCode::Char('u')) };
        let _ = form.key(ctrl_u);
        typed(&mut form, "My Web_1!");
        assert_eq!(form.name.text, "my-web-1");
        // Editing in the middle.
        for code in [KeyCode::Home, KeyCode::Right, KeyCode::Right, KeyCode::Delete, KeyCode::Char('_')] {
            let _ = form.key(press(code));
        }
        assert_eq!(form.name.text, "my-web-1");
        assert_eq!(form.name.cursor, 3);
        let _ = form.key(press(KeyCode::Backspace));
        assert_eq!(form.name.text, "myweb-1");
    }

    #[test]
    fn enter_jumps_to_the_first_problem() {
        let home = home();
        let mut form = form(&home);
        for _ in 0..3 {
            let _ = form.key(press(KeyCode::Tab));
        }
        assert_eq!(form.focus, Field::Memory);
        let _ = form.key(press(KeyCode::Backspace));
        let _ = form.key(press(KeyCode::Backspace));
        typed(&mut form, "8x"); // the x is ignored
        assert_eq!(form.memory.text, "8");
        let _ = form.key(press(KeyCode::Up));
        assert_eq!(form.focus, Field::Image);
        let _ = form.key(press(KeyCode::Up));
        assert_eq!(form.focus, Field::Name);
        let ctrl_u = KeyEvent { modifiers: KeyModifiers::CONTROL, ..press(KeyCode::Char('u')) };
        let _ = form.key(ctrl_u);

        // The empty name comes first.
        assert!(matches!(form.key(press(KeyCode::Enter)), Step::Continue));
        assert_eq!(form.focus, Field::Name);
        assert!(screen(&mut form, 80, 14).0.contains("give it a name"));
        typed(&mut form, "web");
        // Then the memory, which is valid as a bare number of MiB.
        let args = done(form.key(press(KeyCode::Enter)));
        assert_eq!((args.name.as_deref(), args.mem.as_deref()), (Some("web"), Some("8")));
    }

    #[test]
    fn image_list_scrolls_and_jumps() {
        let home = home();
        let mut form = form(&home);
        let _ = form.key(press(KeyCode::Tab));
        let _ = form.key(press(KeyCode::End));
        let (text, _) = screen(&mut form, 80, 14);
        assert!(text.contains("▸ amazonlinux-2023"), "{text}");
        assert!(text.contains("  ↑"), "{text}");
        assert!(!text.contains("debian-13"), "{text}");
        // ↓ past the end moves on to the next field.
        let _ = form.key(press(KeyCode::Down));
        assert_eq!(form.focus, Field::Cpus);
        let _ = form.key(press(KeyCode::BackTab));
        typed(&mut form, "f");
        assert_eq!(form.images[form.selected].arg, "fedora-44");
        let _ = form.key(press(KeyCode::Home));
        assert_eq!(form.selected, 0);
    }

    #[test]
    fn x86_only_images_run_emulated() {
        let home = home();
        let mut form = form(&home);
        let _ = form.key(press(KeyCode::Tab));
        typed(&mut form, "a"); // almalinux-10
        typed(&mut form, "a");
        typed(&mut form, "a"); // archlinux
        assert_eq!(form.images[form.selected].arg, "archlinux");
        assert!(screen(&mut form, 80, 14).0.contains("runs as x86_64 under emulation"));
        let args = done(form.key(press(KeyCode::Enter)));
        // `--arch` is only passed when it differs from the host, which can be either.
        let expected = (Arch::host().unwrap() != Arch::X86_64).then_some(Arch::X86_64);
        assert_eq!(args.arch, expected);
    }

    #[test]
    fn escape_cancels() {
        let home = home();
        assert!(matches!(form(&home).key(press(KeyCode::Esc)), Step::Cancel));
    }

    #[test]
    fn small_terminals_still_draw() {
        let home = home();
        let mut form = form(&home);
        let (text, _) = screen(&mut form, 40, 9);
        assert!(text.starts_with("  Name    dev"), "{text}");
        assert!(text.contains("▸ debian-13"), "{text}");
    }

    #[test]
    fn equivalent_command_line() {
        let defaults = Spec::defaults().unwrap();
        let args = NewArgs {
            name: Some("web".into()),
            image: "fedora-44".into(),
            cpus: Some(defaults.cpus),
            mem: Some("8G".into()),
            arch: None,
            ..args()
        };
        assert_eq!(command_line(&home(), &args).unwrap(), "vx new web --image fedora-44 --mem 8G");
        let args = NewArgs { install: vec!["git".into(), "htop".into()], ..args };
        assert_eq!(command_line(&home(), &args).unwrap(), "vx new web --image fedora-44 --mem 8G --install git,htop");
    }

    #[test]
    fn picker_starts_on_the_preferred_state() {
        let row =
            |name: &str, state| Row { name: name.into(), state: Some(state), detail: "aarch64  4 CPUs  4G".into() };
        let rows = vec![row("api", State::Stopped), row("dev", State::Running), row("web", State::Running)];
        let selected = rows.iter().position(|r| r.state == Some(State::Running)).unwrap();
        let mut picker = Picker { question: "Which VM?".into(), verb: "ssh".into(), rows, selected, scroll: 0 };
        let (text, _) = screen(&mut picker, 60, 5);
        assert_eq!(
            text,
            "  Which VM?
    api  stopped  aarch64  4 CPUs  4G
  ▸ dev  running  aarch64  4 CPUs  4G
    web  running  aarch64  4 CPUs  4G
  ↑↓ choose · enter ssh · esc cancel"
        );
        let _ = picker.key(press(KeyCode::Down));
        let _ = picker.key(press(KeyCode::Down)); // stays on the last one
        assert_eq!(done(picker.key(press(KeyCode::Enter))), "web");
        typed(&mut picker, "a");
        assert_eq!(done(picker.key(press(KeyCode::Enter))), "api");
    }
}
