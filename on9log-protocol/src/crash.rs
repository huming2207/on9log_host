//! Plain-text ESP panic output recognizer and backtrace annotator.

use crate::elf_resolv::ElfStrings;

/// Streaming helper for ESP-IDF panic text carried in plain-text frames.
///
/// Accumulates raw text bytes line-by-line, recognises crash indicators
/// (abort, assert, Guru Meditation, debug exception, stack canary), and
/// resolves backtrace addresses via optional [`ElfStrings`].
pub struct CrashDecoder {
    /// Incomplete current line buffer. Flushed on each `\n`.
    line: Vec<u8>,
}

impl Default for CrashDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl CrashDecoder {
    /// Create a new `CrashDecoder` with an empty line buffer.
    pub fn new() -> Self {
        Self { line: Vec::new() }
    }

    /// Feed raw text bytes and return annotation lines for complete input lines.
    ///
    /// Annotations are produced each time a complete line (ending with `\n`)
    /// is assembled. The line is scanned for known crash indicators and
    /// backtrace address patterns; matching addresses are resolved through
    /// the optional [`ElfStrings`] table.
    pub fn feed(&mut self, bytes: &[u8], elf: Option<&ElfStrings>) -> Vec<String> {
        let mut annotations = Vec::new();
        for &b in bytes {
            self.line.push(b);
            if b == b'\n' {
                self.process_current_line(elf, &mut annotations);
                self.line.clear();
            }
        }
        annotations
    }

    /// Scan the current complete line for crash indicators and backtrace
    /// addresses, appending annotation strings to `out`.
    fn process_current_line(&self, elf: Option<&ElfStrings>, out: &mut Vec<String>) {
        let line = String::from_utf8_lossy(&self.line);
        let line = line.trim_matches(['\r', '\n']);

        if let Some(reason) = crash_reason(line) {
            out.push(format!("--- crash reason: {reason}"));
        }

        if is_abort_pc_line(line)
            && let Some(pc) = first_hex_addr(line)
        {
            out.push(format!("--- abort PC: {}", format_addr(pc, elf)));
        }

        if let Some(backtrace) = line.find("Backtrace:") {
            let pcs = backtrace_pcs(&line[backtrace + "Backtrace:".len()..]);
            for pc in pcs {
                out.push(format!("--- {}", format_addr(pc, elf)));
            }
        }
    }
}

/// Check if `line` contains a known ESP-IDF crash indicator and return the
/// full line text if recognised. Supports abort(), assert, Guru Meditation,
/// Unhandled debug exception, and Stack canary watchpoint messages.
fn crash_reason(line: &str) -> Option<String> {
    if line.contains("abort() was called") {
        return Some(line.to_string());
    }
    if line.contains("assert failed:") {
        return Some(line.to_string());
    }
    if line.contains("Guru Meditation Error:") {
        return Some(line.to_string());
    }
    if line.starts_with("Unhandled debug exception")
        || line.starts_with("Stack canary watchpoint triggered")
    {
        return Some(line.to_string());
    }
    None
}

/// Check if the line is an "abort() was called" line that also contains
/// a " PC " token, indicating the abort program counter is present.
fn is_abort_pc_line(line: &str) -> bool {
    line.contains("abort() was called") && line.contains(" PC ")
}

/// Extract the first hex address from a line, if any.
fn first_hex_addr(line: &str) -> Option<u32> {
    hex_addrs(line).next()
}

/// Extract all backtrace PC addresses from a space-separated list of
/// `pc:sp` pairs (e.g. `0x4037a3ad:0x3fc97b70`).
fn backtrace_pcs(line: &str) -> Vec<u32> {
    line.split_whitespace()
        .filter_map(|token| token.split_once(':').map(|(pc, _)| pc).or(Some(token)))
        .filter_map(parse_hex_addr)
        .collect()
}

/// Iterate over all hex addresses found in a line, scanning for `0x`-prefixed
/// tokens of length 8-16 hex digits.
fn hex_addrs(line: &str) -> impl Iterator<Item = u32> + '_ {
    line.split(|c: char| !(c.is_ascii_hexdigit() || c == 'x' || c == 'X'))
        .filter_map(parse_hex_addr)
}

/// Try to parse a hex address string (with `0x` prefix) into a `u32`.
/// Returns `None` for strings that are not valid 8-16 digit hex values.
fn parse_hex_addr(s: &str) -> Option<u32> {
    let s = s.trim();
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if hex.len() < 8 || hex.len() > 16 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// Format an address with optional ELF symbolication and source location.
///
/// Produces output like `0x42008e7d: app_main+0x14` or
/// `0x42008e7d: app_main+0x14 at main.c:42`.
fn format_addr(addr: u32, elf: Option<&ElfStrings>) -> String {
    let Some(elf) = elf else {
        return format!("0x{addr:08x}: <unresolved>");
    };

    let symbol = match elf.resolve_symbol(addr) {
        Some(sym) if sym.offset == 0 => sym.name.to_string(),
        Some(sym) => format!("{}+0x{:x}", sym.name, sym.offset),
        None => "<unresolved>".to_string(),
    };

    match elf.resolve_location(addr) {
        Some(loc) => match loc.line {
            Some(line) => format!("0x{addr:08x}: {symbol} at {}:{line}", loc.file),
            None => format!("0x{addr:08x}: {symbol} at {}", loc.file),
        },
        None => format!("0x{addr:08x}: {symbol}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_abort_reason_and_pc() {
        let mut decoder = CrashDecoder::new();
        let annotations = decoder.feed(b"abort() was called at PC 0x42008e7d on core 0\n", None);

        assert_eq!(annotations.len(), 2);
        assert!(annotations[0].contains("crash reason"));
        assert_eq!(annotations[1], "--- abort PC: 0x42008e7d: <unresolved>");
    }

    #[test]
    fn extracts_backtrace_pc_half_of_pc_sp_pairs() {
        let mut decoder = CrashDecoder::new();
        let annotations = decoder.feed(
            b"Backtrace: 0x4037a3ad:0x3fc97b70 0x42008e7d:0x3fc97c20\n",
            None,
        );

        assert_eq!(
            annotations,
            vec![
                "--- 0x4037a3ad: <unresolved>",
                "--- 0x42008e7d: <unresolved>",
            ]
        );
    }

    #[test]
    fn waits_for_complete_plain_text_line() {
        let mut decoder = CrashDecoder::new();
        assert!(
            decoder
                .feed(b"Backtrace: 0x4037a3ad:0x3fc97b70", None)
                .is_empty()
        );
        assert_eq!(
            decoder.feed(b"\n", None),
            vec!["--- 0x4037a3ad: <unresolved>"]
        );
    }
}
