//! Interactive forms drawn inline in the terminal, used when a command is missing arguments:
//! `vx new` without a name opens a form, and `vx ssh` without a VM opens a picker.
//!
//! Each form only fills in the same arguments the flags do, then the normal command runs.

use std::io::{self, IsTerminal, Stdout};

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

use crate::backend::{self, State};
use crate::host::Arch;
use crate::image::{self, Source};
use crate::progress::bytes;
use crate::style::OUT;
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
enum Step<T> {
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
                for cell in &mut frame.buffer_mut().content {
                    cell.set_fg(Color::Reset);
                }
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
}

const FIELDS: [Field; 5] = [Field::Name, Field::Image, Field::Cpus, Field::Memory, Field::Disk];

/// One row of the image list.
struct Choice {
    /// What `--image` gets.
    arg: String,
    title: String,
    size: String,
    cached: bool,
    /// Whether it's built for the VM's architecture; if not, it runs as the other one, emulated.
    native: bool,
}

/// The `vx new` form.
struct NewForm<'a> {
    home: &'a Home,
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
}

/// Rows besides the image list: name, blank, blank, sizes, blank, message, help.
const FIXED_ROWS: u16 = 7;
const IMAGE_ROWS: u16 = 7;
const LABEL: usize = 8;
const MARGIN: &str = "  ";

/// Fill in `vx new`'s arguments with a form, starting from the ones given as flags.
/// Returns `None` if cancelled.
pub fn new_vm(home: &Home, args: NewArgs) -> Result<Option<NewArgs>> {
    let mut form = NewForm::new(home, args)?;
    let height = FIXED_ROWS + IMAGE_ROWS.min(form.images.len() as u16);
    run(height, &mut form)
}

impl<'a> NewForm<'a> {
    fn new(home: &'a Home, args: NewArgs) -> Result<NewForm<'a>> {
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
            })
            .collect();
        // `--image ./disk.qcow2` shows up as its own row.
        if let Source::File(path) = Source::parse(&args.image)? {
            let size = path.metadata().map(|m| bytes(m.len())).unwrap_or_default();
            images.insert(0, Choice { arg: args.image.clone(), title: "local file".into(), size, cached: true, native: true });
        }
        let selected = images.iter().position(|c| c.arg == args.image).unwrap_or(0);
        let name = args.name.unwrap_or_else(|| suggest_name(home));
        Ok(NewForm {
            home,
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
            defaults,
        })
    }

    fn input(&mut self, field: Field) -> Option<&mut Input> {
        match field {
            Field::Name => Some(&mut self.name),
            Field::Cpus => Some(&mut self.cpus),
            Field::Memory => Some(&mut self.memory),
            Field::Disk => Some(&mut self.disk),
            Field::Image => None,
        }
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
        }
    }

    fn move_focus(&mut self, by: isize) {
        let i = FIELDS.iter().position(|f| *f == self.focus).unwrap_or(0) as isize;
        self.focus = FIELDS[(i + by).rem_euclid(FIELDS.len() as isize) as usize];
    }

    fn select(&mut self, index: usize) {
        self.selected = index.min(self.images.len() - 1);
    }

    fn handle(&mut self, key: KeyEvent) -> Step<NewArgs> {
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
                }
            }
            KeyCode::Down => {
                self.focus = match self.focus {
                    Field::Name => Field::Image,
                    Field::Image => Field::Cpus,
                    row => row,
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
        })
    }

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
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
        for (field, title, width) in [(Field::Cpus, "CPUs", 6), (Field::Memory, "Memory", 6), (Field::Disk, "Disk", 6)] {
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
                        format!("no {} build, so it runs as {} under emulation (much slower)", self.arch, other(self.arch)),
                        WARN,
                    );
                }
                if choice.cached { "already downloaded".into() } else { format!("downloads {} once, then it's cached", choice.size) }
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
        };
        Span::styled(tip, DIM)
    }
}

impl Form for NewForm<'_> {
    type Output = NewArgs;

    fn draw(&mut self, frame: &mut Frame) {
        self.render(frame);
    }

    fn key(&mut self, key: KeyEvent) -> Step<NewArgs> {
        self.handle(key)
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
pub fn command_line(args: &NewArgs) -> Result<String> {
    let defaults = Spec::defaults()?;
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

    fn form(home: &Home) -> NewForm<'_> {
        NewForm::new(home, NewArgs { cpus: Some(4), ..args() }).unwrap()
    }

    #[test]
    fn draws_the_new_form() {
        let home = home();
        let mut form = form(&home);
        let (text, cursor) = screen(&mut form, 80, 14);
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

  ssh into it later with `vx ssh dev`
  tab/↑↓ move · enter create · esc cancel"
        );
        assert_eq!(cursor, Some((13, 0)), "cursor after `dev`");
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
        assert_eq!(args.arch, Some(Arch::X86_64));
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
        assert_eq!(command_line(&args).unwrap(), "vx new web --image fedora-44 --mem 8G");
    }

    #[test]
    fn picker_starts_on_the_preferred_state() {
        let row = |name: &str, state| Row { name: name.into(), state: Some(state), detail: "aarch64  4 CPUs  4G".into() };
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
