//! How the CLI writes to a terminal: colors, symbols, columns that fit its width. Written
//! anywhere else (a pipe, a file, a log), or with `NO_COLOR` set, the output stays plain text.

use std::fmt::Display;
use std::io::IsTerminal;

/// Styles text for one output stream, or leaves it plain.
#[derive(Clone, Copy)]
pub struct Paint {
    pub on: bool,
}

/// For standard output.
pub fn out() -> Paint {
    Paint {
        on: wanted(std::io::stdout().is_terminal()),
    }
}

/// For standard error.
pub fn err() -> Paint {
    Paint {
        on: wanted(std::io::stderr().is_terminal()),
    }
}

/// The usual conventions: NO_COLOR (no-color.org) turns color off, CLICOLOR_FORCE on.
fn wanted(terminal: bool) -> bool {
    let set = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0");
    if set("NO_COLOR") {
        return false;
    }
    if set("CLICOLOR_FORCE") {
        return true;
    }
    terminal && std::env::var_os("TERM").is_none_or(|t| t != "dumb")
}

impl Paint {
    fn style(self, code: &str, text: impl Display) -> String {
        if self.on {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn bold(self, text: impl Display) -> String {
        self.style("1", text)
    }

    pub fn dim(self, text: impl Display) -> String {
        self.style("2", text)
    }

    pub fn red(self, text: impl Display) -> String {
        self.style("31", text)
    }

    pub fn green(self, text: impl Display) -> String {
        self.style("32", text)
    }

    pub fn yellow(self, text: impl Display) -> String {
        self.style("33", text)
    }

    pub fn cyan(self, text: impl Display) -> String {
        self.style("36", text)
    }

    /// Marks a line that says something worked: "✓ " on a terminal, `plain` elsewhere.
    pub fn ok(self, plain: &str) -> String {
        if self.on {
            format!("{} ", self.green("✓"))
        } else {
            plain.to_owned()
        }
    }

    /// Marks a line that says something failed: "✗ " on a terminal, `plain` elsewhere.
    pub fn fail(self, plain: &str) -> String {
        if self.on {
            format!("{} ", self.red("✗"))
        } else {
            plain.to_owned()
        }
    }

    /// Marks a line to pay attention to: "! " on a terminal, `plain` elsewhere.
    pub fn warn(self, plain: &str) -> String {
        if self.on {
            format!("{} ", self.yellow("!"))
        } else {
            plain.to_owned()
        }
    }

    /// `text` broken into lines of at most `width` columns, all but the first indented by
    /// `indent` columns. Only on a terminal: elsewhere it stays one line.
    pub fn wrap(self, text: &str, width: usize, indent: usize) -> String {
        if !self.on || text.chars().count() + indent <= width {
            return text.to_owned();
        }
        let room = width.saturating_sub(indent).max(20);
        let mut lines = vec![String::new()];
        for word in text.split(' ') {
            let line = lines.last_mut().expect("never empty");
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > room {
                lines.push(word.to_owned());
            } else {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
            }
        }
        lines.join(&format!("\n{}", " ".repeat(indent)))
    }
}

/// Columns of the terminal on standard output (80 if it does not say).
pub fn width() -> usize {
    #[cfg(unix)]
    {
        // SAFETY: TIOCGWINSZ only fills in the winsize it is given.
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
            && size.ws_col > 0
        {
            return size.ws_col.into();
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(80)
}

#[cfg(test)]
mod tests {
    use super::Paint;

    #[test]
    fn plain_unless_on() {
        let plain = Paint { on: false };
        assert_eq!(plain.green("x"), "x");
        assert_eq!(plain.ok("ok    "), "ok    ");
        assert_eq!(plain.wrap("one two three", 5, 2), "one two three");
        let on = Paint { on: true };
        assert_eq!(on.green("x"), "\x1b[32mx\x1b[0m");
    }

    #[test]
    fn wraps_with_indent() {
        let on = Paint { on: true };
        let text = "nothing listens on 127.0.0.1:3845 (connection refused): is the program \
                    that serves it running?";
        let wrapped = on.wrap(text, 40, 4);
        for line in wrapped.lines() {
            assert!(line.chars().count() <= 40, "{line:?}");
        }
        assert!(wrapped.lines().skip(1).all(|l| l.starts_with("    ")));
        assert_eq!(
            wrapped.split_whitespace().collect::<Vec<_>>().join(" "),
            text
        );
    }
}
