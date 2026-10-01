//! One-line progress output on stderr: a download bar, and a spinner with a status message.
//! When stderr isn't a terminal, only start and end lines are printed.

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

const REDRAW: Duration = Duration::from_millis(100);
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// `  ↓ debian-13  ██████████░░░░░░░░░░  48%  156 MB/337 MB   20 MB/s   0:09`
pub struct Bar {
    label: String,
    total: Option<u64>,
    done: u64,
    start: Instant,
    drawn: Option<Instant>,
    /// Fixed for the bar's life, so the line doesn't jitter as numbers change.
    width: usize,
    tty: bool,
}

/// The widest the stats after the bar get: `100%  337 MB/337 MB  100 MB/s  59:59`.
const STATS: usize = 4 + 2 + 15 + 2 + 8 + 2 + 5;

impl Bar {
    pub fn new(label: &str, total: Option<u64>) -> Bar {
        let tty = io::stderr().is_terminal();
        if !tty {
            let size = total.map(|t| format!(" ({})", bytes(t))).unwrap_or_default();
            eprintln!("  ↓ downloading {label}{size}");
        }
        let width = term_width().saturating_sub(4 + label.chars().count() + 2 + 1 + STATS + 1).clamp(10, 40);
        Bar { label: label.into(), total, done: 0, start: Instant::now(), drawn: None, width, tty }
    }

    pub fn add(&mut self, n: u64) {
        self.done += n;
        if self.tty && self.drawn.is_none_or(|t| t.elapsed() >= REDRAW) {
            self.drawn = Some(Instant::now());
            redraw(&self.line(false));
        }
    }

    /// Replace the bar with its final line.
    pub fn finish(self) {
        if self.tty {
            eprint!("\r\x1b[2K");
        }
        eprintln!("{}", self.line(true));
    }

    fn line(&self, finished: bool) -> String {
        let elapsed = self.start.elapsed();
        let rate = bytes((self.done as f64 / elapsed.as_secs_f64().max(0.001)) as u64) + "/s";
        let icon = if finished { '✓' } else { '↓' };
        let head = format!("  {icon} {}", self.label);
        let Some(total) = self.total else {
            return format!("{head}  {}  {rate}", bytes(self.done));
        };
        let frac = (self.done as f64 / total.max(1) as f64).min(1.0);
        let bar = meter(frac, self.width);
        let pct = (frac * 100.0) as u32;
        if finished {
            return format!("{head}  {bar} {pct:>3}%  {} in {}", bytes(self.done), clock(elapsed));
        }
        let speed = self.done as f64 / elapsed.as_secs_f64().max(0.001);
        let eta = total.saturating_sub(self.done) as f64 / speed.max(1.0);
        // Unknown until data flows; capped so it never outgrows its column.
        let eta = if self.done == 0 || eta >= 6000.0 { "--:--".into() } else { clock(Duration::from_secs_f64(eta)) };
        let amount = format!("{}/{}", bytes(self.done), bytes(total));
        format!("{head}  {bar} {pct:>3}%  {amount:>15}  {rate:>8}  {eta:>5}")
    }
}

fn meter(frac: f64, width: usize) -> String {
    let full = (frac * width as f64).round() as usize;
    format!("{}{}", "█".repeat(full), "░".repeat(width - full))
}

/// `  ⠋ booting… [  OK  ] Reached target cloud-init.target`
pub struct Spinner {
    frame: usize,
    tty: bool,
}

impl Spinner {
    pub fn new() -> Spinner {
        Spinner { frame: 0, tty: io::stderr().is_terminal() }
    }

    pub fn update(&mut self, msg: &str) {
        if self.tty {
            self.frame += 1;
            redraw(&format!("  {} {msg}", SPINNER[self.frame % SPINNER.len()]));
        }
    }

    /// Erase the spinner line.
    pub fn clear(&self) {
        if self.tty {
            eprint!("\r\x1b[2K");
            let _ = io::stderr().flush();
        }
    }
}

fn redraw(line: &str) {
    let line: String = line.chars().take(term_width()).collect();
    eprint!("\r\x1b[2K{line}");
    let _ = io::stderr().flush();
}

pub fn term_width() -> usize {
    // SAFETY: winsize is plain data, and TIOCGWINSZ only writes into it.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 { ws.ws_col as usize } else { 80 }
}

/// Text without ANSI escape sequences or control characters, so guest output fits on one line.
pub fn plain(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI: ESC [ parameters… final byte in @..~
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

pub fn bytes(n: u64) -> String {
    match n {
        0..1_000_000 => format!("{} kB", n / 1000),
        1_000_000..1_000_000_000 => format!("{} MB", n / 1_000_000),
        _ => format!("{:.1} GB", n as f64 / 1e9),
    }
}

/// `1:05`
pub fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_fits_80_columns_at_a_steady_width() {
        let mut bar = Bar::new("debian-13", Some(337_510_400));
        bar.start = Instant::now() - Duration::from_secs(10);
        bar.width = 80usize.saturating_sub(4 + 9 + 2 + 1 + STATS + 1).clamp(10, 40);
        let mut widths = Vec::new();
        for done in [0, 8_000, 150_000_000, 337_510_400] {
            bar.done = done;
            let line = bar.line(false);
            assert!(line.chars().count() <= 80, "{line}");
            widths.push(line.chars().count());
        }
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    }

    #[test]
    fn meters() {
        assert_eq!(meter(0.0, 4), "░░░░");
        assert_eq!(meter(0.5, 4), "██░░");
        assert_eq!(meter(1.0, 4), "████");
    }

    #[test]
    fn plain_strips_escapes() {
        assert_eq!(plain("\x1b[1m\x1b[33mShell> \x1b[0m\r"), "Shell> ");
        assert_eq!(plain("[  \x1b[0;32mOK\x1b[0m  ] Reached target"), "[  OK  ] Reached target");
    }

    #[test]
    fn sizes_and_clocks() {
        assert_eq!(bytes(325_123_456), "325 MB");
        assert_eq!(bytes(1_234_000_000), "1.2 GB");
        assert_eq!(bytes(12_000), "12 kB");
        assert_eq!(clock(Duration::from_secs(65)), "1:05");
    }
}
