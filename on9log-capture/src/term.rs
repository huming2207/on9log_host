//! Terminal helpers for `on9log-capture`: ANSI colors, word wrap, and timestamp
//! formatting.
//!
//! These mirror `on9log-cli`'s `term.rs` but add portable host-time formatting
//! (`local_ts_string`) used to stamp each captured event. The timestamp is
//! always derived from a host wall-clock millisecond value captured at receive
//! time and stored in SQLite.

use std::io::{IsTerminal, Write};

/// Whether stdout is an interactive terminal (so colors should be emitted).
pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
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

/// Print a colored, wrapped log line to stdout. `width == 0` disables wrapping
/// (the line is printed whole), which keeps decode output grep- and
/// file-friendly. `use_color` controls ANSI emission.
pub fn print_log_line(
    prefix: &str,
    message: &str,
    color_code: &str,
    indent: usize,
    width: usize,
    use_color: bool,
) {
    if width == 0 {
        let out = std::io::stdout();
        let mut h = out.lock();
        if use_color {
            let _ = writeln!(
                h,
                "{color_code}{BOLD}{prefix}{RESET}{color_code}{msg}{RESET}",
                color_code = color_code,
                BOLD = color::BOLD,
                RESET = color::RESET,
                msg = message
            );
        } else {
            let _ = writeln!(h, "{prefix}{message}");
        }
        return;
    }

    let prefix_visible = prefix.chars().count();
    let first_avail = width.saturating_sub(prefix_visible);
    let avail = width.saturating_sub(indent).max(1);

    let wrapped = if first_avail > 0 {
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

/// Host wall clock in milliseconds since the Unix epoch (UTC). Used as the
/// per-event capture timestamp stored in SQLite.
pub fn host_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Format a host wall-clock millisecond value as a local timestamp string
/// `YYYYMMDD-HH:MM:SS.mmm`. On Unix this uses `localtime_r` so it respects the
/// host timezone; elsewhere it falls back to a UTC breakdown.
pub fn local_ts_string(unix_ms: u64) -> String {
    if unix_ms == 0 {
        return "--------------------.---".to_string();
    }
    let secs = (unix_ms / 1000) as i64;
    let millis = (unix_ms % 1000) as i64;
    format_ts(secs, millis)
}

/// Format a Unix timestamp (seconds + milliseconds) as
/// `YYYYMMDD-HH:MM:SS.mmm` using `localtime_r` (Unix variant). Falls back to
/// `@<secs>s` if `localtime_r` fails.
#[cfg(unix)]
fn format_ts(secs: i64, millis: i64) -> String {
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    let t: libc::time_t = secs as libc::time_t;
    let tm = unsafe {
        if libc::localtime_r(&t, tm.as_mut_ptr()).is_null() {
            return format!("@{}s", secs);
        }
        tm.assume_init()
    };
    format!(
        "{:04}{:02}{:02}-{:02}:{:02}:{:02}.{:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        millis
    )
}

/// Format a Unix timestamp as `YYYYMMDD-HH:MM:SS.mmmZ` (UTC-only fallback
/// for non-Unix platforms where `localtime_r` is unavailable).
#[cfg(not(unix))]
fn format_ts(secs: i64, millis: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;
    format!(
        "{:04}{:02}{:02}-{:02}:{:02}:{:02}.{:03}Z",
        y, m, d, hour, min, sec, millis
    )
}

/// Convert a days-from-Unix-epoch value into a `(year, month, day)` civil date
/// using Howard Hinnant's algorithm (UTC-only fallback, no `localtime_r`
/// required). Returns a proleptic Gregorian date.
#[cfg(not(unix))]
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_zero_width_returns_single_line() {
        let lines = wrap("the quick brown fox", 0);
        assert_eq!(lines, vec!["the quick brown fox".to_string()]);
    }

    #[test]
    fn wrap_long_line_breaks() {
        let lines = wrap("the quick brown fox jumps over the lazy dog", 10);
        assert!(lines.len() > 1);
        for l in &lines {
            assert!(l.chars().count() <= 10);
        }
    }

    #[test]
    fn local_ts_string_formats_known_epoch() {
        // 2024-01-01T00:00:00Z == 1704067200000 ms.
        let s = local_ts_string(1_704_067_200_000);
        // "YYYYMMDD-HH:MM:SS.mmm" is 21 chars (22 with the trailing 'Z' UTC
        // fallback). The date/time separator is at byte index 8.
        assert!(s.len() >= 21);
        assert_eq!(s.as_bytes()[8], b'-');
        assert!(s.as_bytes()[4].is_ascii_digit());
    }
}
