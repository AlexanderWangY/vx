//! Terminal colors, used sparingly and with one meaning each: green for success, yellow for
//! warnings, red for errors, cyan for progress and hints, dim for secondary text.
//! Off when the stream isn't a terminal, NO_COLOR is set, or TERM is dumb.

use std::env;
use std::fmt::{self, Display};
use std::io::{self, IsTerminal};
use std::sync::OnceLock;

/// Colors for one output stream; each decides on its own, so `vx ls | less` gets plain text.
#[derive(Clone, Copy)]
pub struct Colors {
    stderr: bool,
}

pub const OUT: Colors = Colors { stderr: false };
pub const ERR: Colors = Colors { stderr: true };

impl Colors {
    pub fn enabled(self) -> bool {
        static OUT_ON: OnceLock<bool> = OnceLock::new();
        static ERR_ON: OnceLock<bool> = OnceLock::new();
        let wanted = || {
            env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()) && env::var("TERM").map_or(true, |t| t != "dumb")
        };
        if self.stderr {
            *ERR_ON.get_or_init(|| wanted() && io::stderr().is_terminal())
        } else {
            *OUT_ON.get_or_init(|| wanted() && io::stdout().is_terminal())
        }
    }

    fn paint<T: Display>(self, code: &'static str, text: T) -> Paint<T> {
        Paint { text, code: if self.enabled() { code } else { "" } }
    }

    pub fn green<T: Display>(self, text: T) -> Paint<T> {
        self.paint("32", text)
    }

    pub fn yellow<T: Display>(self, text: T) -> Paint<T> {
        self.paint("33", text)
    }

    pub fn red<T: Display>(self, text: T) -> Paint<T> {
        self.paint("31", text)
    }

    pub fn cyan<T: Display>(self, text: T) -> Paint<T> {
        self.paint("36", text)
    }

    pub fn dim<T: Display>(self, text: T) -> Paint<T> {
        self.paint("2", text)
    }

    pub fn bold<T: Display>(self, text: T) -> Paint<T> {
        self.paint("1", text)
    }

    /// `error:`, `warning:` and `hint:` labels.
    pub fn label<T: Display>(self, kind: Label, text: T) -> Paint<T> {
        self.paint(
            match kind {
                Label::Error => "1;31",
                Label::Warning => "1;33",
                Label::Hint => "1;36",
            },
            text,
        )
    }
}

#[derive(Clone, Copy)]
pub enum Label {
    Error,
    Warning,
    Hint,
}

/// `warning: …` on stderr, after `indent`.
pub fn warn(indent: &str, msg: impl Display) {
    eprintln!("{indent}{} {msg}", ERR.label(Label::Warning, "warning:"));
}

/// Text wrapped in an ANSI style. Width and alignment (`{:8}`) apply to the text itself.
pub struct Paint<T> {
    text: T,
    code: &'static str,
}

impl<T: Display> Display for Paint<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.code.is_empty() {
            return self.text.fmt(f);
        }
        write!(f, "\x1b[{}m", self.code)?;
        self.text.fmt(f)?;
        f.write_str("\x1b[0m")
    }
}

/// Drop foreground colors from a drawn screen, keeping bold and dim, for NO_COLOR.
pub fn strip_colors(buffer: &mut ratatui::buffer::Buffer) {
    for cell in &mut buffer.content {
        cell.set_fg(ratatui::style::Color::Reset);
    }
}

/// Cut `line` to `width` visible columns, skipping over escape sequences, so a long line
/// redrawn with `\r` never wraps and never loses its closing reset.
pub fn fit(line: &str, width: usize) -> String {
    let mut out = String::with_capacity(line.len());
    let mut shown = 0;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            for c in chars.by_ref() {
                out.push(c);
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if shown < width {
            out.push(c);
            shown += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_applies_inside_the_color() {
        let p = Paint { text: "running", code: "32" };
        assert_eq!(format!("{p:9}|"), "\x1b[32mrunning  \x1b[0m|");
        let plain = Paint { text: "running", code: "" };
        assert_eq!(format!("{plain:>9}|"), "  running|");
    }

    #[test]
    fn fit_counts_only_visible_characters() {
        let line = "\x1b[36m↓\x1b[0m debian-13  \x1b[36m███\x1b[0m\x1b[2m░░░\x1b[0m";
        assert_eq!(fit(line, 100), line);
        // Cut inside the bar, but every escape sequence is kept, so styles still reset.
        assert_eq!(fit(line, 14), "\x1b[36m↓\x1b[0m debian-13  \x1b[36m█\x1b[0m\x1b[2m\x1b[0m");
    }
}
