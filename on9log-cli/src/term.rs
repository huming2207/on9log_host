//! Terminal helpers: color ANSI codes and virtual-terminal width detection.
//!
//! Width detection uses `crossterm::terminal::size()`, which handles the
//! platform-specific `ioctl(TIOCGWINSZ)` for us on Linux and macOS. We fall
//! back to the `COLUMNS` env var, then a conservative default of 80.

use std::io::{IsTerminal, Write};

/// Current terminal column count for stdout, or a default if undetectable.
pub fn terminal_width() -> usize {
    if let Ok((cols, _)) = crossterm::terminal::size()
        && cols > 0
    {
        return cols as usize;
    }
    if let Ok(cols) = std::env::var("COLUMNS")
        && let Ok(c) = cols.parse::<usize>()
        && c > 0
    {
        return c;
    }
    80
}

/// Whether stdout is an interactive terminal (so colors should be emitted).
pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// Whether stdin is an interactive terminal (so monitor control keys can be read).
pub fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// ANSI SGR color codes used for log levels.
pub mod color {
    /// Reset all ANSI attributes to default.
    pub const RESET: &str = "\x1b[0m";
    /// Red foreground (typically used for `Level::Error`).
    pub const RED: &str = "\x1b[31m";
    /// Yellow foreground (typically used for `Level::Warn`).
    pub const YELLOW: &str = "\x1b[33m";
    /// Green foreground (typically used for `Level::Info`).
    pub const GREEN: &str = "\x1b[32m";
    /// White / default foreground (used for `Level::Debug`/`Level::Verbose`).
    pub const WHITE: &str = "\x1b[37m";
    /// Bold / increased intensity.
    pub const BOLD: &str = "\x1b[1m";
    /// Dim / reduced intensity (used for warning prose).
    pub const DIM: &str = "\x1b[2m";
}

/// Word-wrap `text` to `width` columns, returning the wrapped lines (without
/// trailing newlines). Long unbreakable tokens are hard-broken at `width`.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut line_len = 0usize;
        for word in paragraph.split_whitespace() {
            let wlen = word.chars().count();
            if line_len == 0 {
                if wlen > width {
                    push_hard(&mut lines, &mut line, &mut line_len, word, width);
                } else {
                    line.push_str(word);
                    line_len = wlen;
                }
            } else if line_len + 1 + wlen <= width {
                line.push(' ');
                line.push_str(word);
                line_len += 1 + wlen;
            } else {
                lines.push(std::mem::take(&mut line));
                if wlen > width {
                    push_hard(&mut lines, &mut line, &mut line_len, word, width);
                } else {
                    line.push_str(word);
                    line_len = wlen;
                }
            }
        }
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Hard-break a word that is wider than `width` by pushing one character at a
/// time onto the current line, wrapping to a new line whenever the width is
/// exhausted.
fn push_hard(
    lines: &mut Vec<String>,
    line: &mut String,
    line_len: &mut usize,
    word: &str,
    width: usize,
) {
    let mut chars = word.chars().peekable();
    while chars.peek().is_some() {
        while *line_len < width && chars.peek().is_some() {
            let c = chars.next().unwrap();
            line.push(c);
            *line_len += 1;
        }
        if chars.peek().is_some() {
            lines.push(std::mem::take(line));
            *line_len = 0;
        }
    }
}

/// Print a colored, wrapped log line to stdout. The prefix and message share
/// the level color; subsequent wrapped lines are indented to align under the
/// message. `use_color` controls ANSI emission.
pub fn print_log_line(
    prefix: &str,
    message: &str,
    color_code: &str,
    indent: usize,
    width: usize,
    use_color: bool,
) {
    let prefix_visible = prefix.chars().count();
    let first_avail = width.saturating_sub(prefix_visible);
    let avail = width.saturating_sub(indent).max(1);

    let wrapped = if first_avail > 0 {
        // Wrap first line to remaining width, later lines to full avail.
        let mut all = wrap(message, first_avail.max(1));
        if all.len() > 1 {
            let rest = all.split_off(1);
            for r in rest {
                all.extend(wrap(&r, avail));
            }
        }
        all
    } else {
        wrap(message, avail)
    };

    let pad = " ".repeat(indent);
    for (i, ln) in wrapped.iter().enumerate() {
        let out = std::io::stdout();
        let mut h = out.lock();
        if i == 0 {
            if use_color {
                let _ = writeln!(
                    h,
                    "{color_code}{BOLD}{prefix}{RESET}{color_code}{ln}{RESET}",
                    color_code = color_code,
                    BOLD = color::BOLD,
                    prefix = prefix,
                    RESET = color::RESET,
                    ln = ln
                );
            } else {
                let _ = writeln!(h, "{prefix}{ln}");
            }
        } else if use_color {
            let _ = writeln!(
                h,
                "{pad}{color_code}{ln}{RESET}",
                pad = pad,
                color_code = color_code,
                RESET = color::RESET,
                ln = ln
            );
        } else {
            let _ = writeln!(h, "{pad}{ln}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_long_line() {
        let lines = wrap("the quick brown fox jumps over the lazy dog", 10);
        assert!(lines.len() > 1);
        for l in &lines {
            assert!(l.chars().count() <= 10);
        }
    }

    #[test]
    fn empty_input_returns_empty_line() {
        let lines = wrap("", 10);
        assert_eq!(lines, vec!["".to_string()]);
    }
}
