//! `on9log-capture` — capture on9log streams to SQLite and replay them later.
//!
//! Two subcommands split the customer / developer workflow:
//!
//! - `capture` (customer): opens a UART, deframes the typed SLIP+CRC transport
//!   with `on9log_protocol::Deframer`, and stores every raw outcome — on9log
//!   binary frames, SLIP-framed plain text, raw plain text, and deframer
//!   diagnostics — to a SQLite database, each stamped with the host wall-clock
//!   millisecond at which it was received. No ELF is accepted: the customer does
//!   not have the firmware ELF with the secret format strings, so nothing is
//!   decoded against an ELF at capture time and captured log contents are never
//!   printed by this subcommand.
//!
//! - `decode` (developer): opens a captured database (and optionally the matching
//!   firmware `--elf`), replays the stored outcomes in capture order through the
//!   same `Decoder` / `CrashDecoder` pipeline the live `on9log` CLI uses, and
//!   prints human-readable, optionally colored text. With `--save` the rendered
//!   text is written to a file instead of stdout.
//!
//! Storage is deliberately ELF-independent: what the customer ships is raw
//! header+payload bytes, so decoding is redone by whoever holds the ELF.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use tokio::io::AsyncReadExt;
use tokio_serial::{SerialPort, SerialPortBuilderExt};

use on9log_protocol::{CrashDecoder, DecodedPacket, Decoder, Deframer, ElfStrings, Level, Outcome};

mod db;
mod term;

use db::CaptureDb;
use term::color;
use term::{host_now_ms, local_ts_string, print_log_line, stdout_is_tty};

/// Capture on9log streams to SQLite, and replay captured databases against an ELF.
#[derive(Parser, Debug)]
#[command(name = "on9log-capture", version, about)]
struct Cli {
    /// Subcommand: `capture` (customer workflow) or `decode` (developer workflow).
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Capture a live UART stream to a SQLite database (customer workflow).
    Capture(CaptureArgs),
    /// Decode a captured SQLite database to human-readable text (developer workflow).
    Decode(DecodeArgs),
}

#[derive(Parser, Debug)]
struct CaptureArgs {
    /// UART port device path, e.g. /dev/ttyUSB0.
    #[arg(short, long, value_name = "PORT")]
    port: String,

    /// Baud rate.
    #[arg(short, long, value_name = "BAUD", default_value_t = 115_200)]
    baud: u32,

    /// SQLite output path. Defaults to on9log-capture-UNIX_TIMESTAMP.sqlite.
    #[arg(short = 'o', long, value_name = "DB")]
    out: Option<PathBuf>,

    /// Do not reset ESP targets by toggling DTR/RTS when the port opens.
    #[arg(long)]
    no_esp_reset: bool,
}

#[derive(Parser, Debug)]
struct DecodeArgs {
    /// Previously captured SQLite database to decode.
    #[arg(value_name = "DB")]
    db: PathBuf,

    /// Firmware ELF used to resolve format/tag string addresses and symbols.
    #[arg(long, value_name = "FILE")]
    elf: Option<PathBuf>,

    /// Disable colored output.
    #[arg(long)]
    no_color: bool,

    /// Prefix each line with the captured-at local timestamp.
    #[arg(short = 't', long)]
    timestamp: bool,

    /// Wrap output to this width (0 = no wrapping; grep/file friendly).
    #[arg(long, default_value_t = 0)]
    width: usize,

    /// Write decoded human-readable text to a file instead of stdout.
    #[arg(short = 's', long, value_name = "FILE")]
    save: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Capture(args) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(capture_run(args))?;
        }
        Cmd::Decode(args) => {
            decode_run(args)?;
        }
    }
    Ok(())
}

/// Load an ELF file for string-address resolution. Returns `None` (with an
/// info message) when no path is provided, or an error if the path exists but
/// cannot be parsed.
fn load_elf(
    path: Option<&std::path::Path>,
) -> Result<Option<ElfStrings>, Box<dyn std::error::Error>> {
    match path {
        Some(p) => match ElfStrings::from_path(p) {
            Ok(e) => {
                eprintln!("on9log-capture: loaded ELF {}", p.display());
                Ok(Some(e))
            }
            Err(e) => Err(format!("failed to parse ELF {}: {e}", p.display()).into()),
        },
        None => {
            eprintln!("on9log-capture: no --elf given; format/tag addresses render as hex");
            Ok(None)
        }
    }
}

/// Capture loop: opens a UART serial port, deframes all incoming outcomes,
/// stamps each with the host receive time, and stores them in a SQLite
/// database. Runs until EOF on the serial port or the user presses Ctrl+C.
///
/// # Errors
/// Returns an error if the serial port cannot be opened, the ESP reset fails,
/// or an I/O error occurs during the read loop.
async fn capture_run(args: CaptureArgs) -> Result<(), Box<dyn std::error::Error>> {
    let out_path = args.out.clone().unwrap_or_else(default_capture_path);
    let mut db = CaptureDb::open(&out_path)?;
    db.write_meta("port", &args.port)?;
    db.write_meta("baud", &args.baud.to_string())?;
    db.write_meta("capture_started_ms", &host_now_ms().to_string())?;
    db.write_meta(
        "tool",
        concat!("on9log-capture ", env!("CARGO_PKG_VERSION")),
    )?;

    let mut serial = tokio_serial::new(&args.port, args.baud)
        .open_native_async()
        .map_err(|e| format!("opening {}: {e}", args.port))?;

    if !args.no_esp_reset {
        esp_hard_reset(&mut serial)
            .map_err(|e| format!("resetting ESP target on {}: {e}", args.port))?;
    }

    let mut deframer = Deframer::new();
    let mut buf = vec![0u8; 4096];

    let mut total: u64 = db.event_count()? as u64;
    let mut since_print: u64 = 0;
    let port = &args.port;
    let baud = args.baud;
    let out = out_path.display();

    eprintln!(
        "on9log-capture: capturing {port} @ {baud} baud -> {out} ({total} existing events, esp-reset {reset}, no log preview, quit Ctrl+C)",
        reset = if args.no_esp_reset { "off" } else { "on" },
    );

    loop {
        let n = tokio::select! {
            serial_read = serial.read(&mut buf) => serial_read?,
            ctrl_c = tokio::signal::ctrl_c() => {
                ctrl_c?;
                eprintln!("on9log-capture: Ctrl+C received, stopping capture");
                break;
            }
        };
        if n == 0 {
            // EOF on the device; stop.
            break;
        }
        let outcomes = deframer.feed(&buf[..n]);
        if outcomes.is_empty() {
            continue;
        }
        // Stamp each outcome with its receive time before storing, so the
        // timestamp reflects when the packet arrived rather than commit time.
        let stamped: Vec<(Outcome, u64)> =
            outcomes.into_iter().map(|o| (o, host_now_ms())).collect();
        db.store_outcomes(&stamped)?;
        total += stamped.len() as u64;

        since_print += stamped.len() as u64;
        if since_print >= 100 {
            eprintln!("on9log-capture: {total} events captured so far...");
            since_print = 0;
        }
    }

    eprintln!("on9log-capture: stopped, {total} events in {out}");
    Ok(())
}

/// Decode loop: opens a previously captured SQLite database, replays every
/// stored event through the on9log decode pipeline, and renders human-readable
/// output to stdout (or a save file). Optionally loads an ELF for string
/// resolution.
///
/// # Errors
/// Returns an error if the database cannot be opened, no `events` table is
/// found, or the ELF file cannot be parsed.
fn decode_run(args: DecodeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let elf = load_elf(args.elf.as_deref())?;
    let db = CaptureDb::open_readonly(&args.db)?;
    let count = db.event_count()?;
    eprintln!(
        "on9log-capture: decoding {count} events from {}",
        args.db.display()
    );

    let to_stdout = args.save.is_none();
    let use_color = !args.no_color && to_stdout && stdout_is_tty();
    // Decode defaults to no wrapping so output stays grep- and file-friendly.
    let width = if args.width > 0 { args.width } else { 0 };
    let save = match &args.save {
        Some(p) => Some(SaveLog::create(p)?),
        None => None,
    };
    let mut renderer = Renderer::new(
        RenderOptions {
            use_color,
            width,
            timestamp: args.timestamp,
            to_stdout,
            strip_ansi: args.no_color,
        },
        save,
    );

    db.for_each_event(|outcome, captured_at_ms| {
        renderer.handle(outcome, elf.as_ref(), captured_at_ms);
        Ok(())
    })?;
    Ok(())
}

/// Generate a default capture database path:
/// `on9log-capture-{unix_epoch_seconds}.sqlite`.
fn default_capture_path() -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    PathBuf::from(format!("on9log-capture-{ts}.sqlite"))
}

// ---------------------------------------------------------------------------
// Rendering for decode replay
// ---------------------------------------------------------------------------

/// Rendering options for the decode-replay loop.
#[derive(Clone, Copy)]
struct RenderOptions {
    /// Whether to emit ANSI color codes.
    use_color: bool,
    /// Line-wrap width (0 = no wrapping, grep-friendly).
    width: usize,
    /// Whether to prefix each line with the captured-at local timestamp.
    timestamp: bool,
    /// When true, output goes to stdout; when false, only the save-file path.
    to_stdout: bool,
    /// Whether to strip ANSI escape sequences from device plain text before
    /// outputting (used when `--no-color` is active or when saving to file).
    strip_ansi: bool,
}

/// Stateful renderer wrapping the on9log decode pipeline. The timestamp source
/// is always the per-event `captured_at_ms` stored during capture.
struct Renderer {
    /// Rendering configuration (color, width, timestamp, etc.).
    opts: RenderOptions,
    /// Protocol decoder for on9log binary frames.
    decoder: Decoder,
    /// Crash decoder for extracting backtrace annotations from plain text.
    crash: CrashDecoder,
    /// Plain-text state for stdout output (tracks timestamps at line starts).
    plain: PlainTextState,
    /// Separate plain-text state for the save-file path.
    save_plain: PlainTextState,
    /// Optional save-file writer.
    save: Option<SaveLog>,
}

impl Renderer {
    /// Create a new renderer with the given options and optional save file.
    fn new(opts: RenderOptions, save: Option<SaveLog>) -> Self {
        Self {
            opts,
            decoder: Decoder::new(),
            crash: CrashDecoder::new(),
            plain: PlainTextState::new(),
            save_plain: PlainTextState::new(),
            save,
        }
    }

    /// Process a single [`Outcome`] through the decode pipeline and render it
    /// to stdout and/or the save file. `captured_at_ms` is the wall-clock
    /// receive timestamp from the capture database.
    fn handle(&mut self, outcome: &Outcome, elf: Option<&ElfStrings>, captured_at_ms: u64) {
        let RenderOptions {
            use_color,
            width,
            timestamp,
            to_stdout,
            strip_ansi,
        } = self.opts;
        match outcome {
            Outcome::Frame(frame) => match self.decoder.decode(frame, elf) {
                DecodedPacket::Log(l) => {
                    let color_code = level_color(l.level);
                    let prefix = format!(
                        "{}{} ({:>7}) {}: ",
                        ts_prefix(timestamp, captured_at_ms),
                        l.level.letter(),
                        l.meta.time_ms,
                        l.tag
                    );
                    let indent = prefix.chars().count();
                    if let Some(gap) = l.meta.gap {
                        let msg =
                            format!("--- missed {gap} packet(s) before seq {} ---", l.meta.seq);
                        self.warn_line(&msg, captured_at_ms);
                    }
                    let message = clean_string(&l.message, strip_ansi);
                    if to_stdout {
                        print_log_line(&prefix, &message, color_code, indent, width, use_color);
                    }
                    if let Some(save) = self.save.as_mut() {
                        let _ = save.write_line(&format!("{prefix}{message}"));
                    }
                    self.plain.reset_line();
                    self.save_plain.reset_line();
                }
                DecodedPacket::Dropped(d) => {
                    let msg = format!(
                        "--- device dropped {} packet(s) at seq {} (t={}ms) ---",
                        d.count, d.meta.seq, d.meta.time_ms
                    );
                    self.warn_line(&msg, captured_at_ms);
                    self.plain.reset_line();
                    self.save_plain.reset_line();
                }
                DecodedPacket::Buffer(b) => {
                    let color_code = level_color(b.level);
                    let header = format!(
                        "{}{} ({:>7}) {} [buf {} @{} +{}]: <buffer dump, {} bytes>",
                        ts_prefix(timestamp, captured_at_ms),
                        b.level.letter(),
                        b.meta.time_ms,
                        b.tag,
                        b.total_len,
                        b.offset,
                        b.bytes.len(),
                        b.bytes.len()
                    );
                    if to_stdout {
                        if use_color {
                            println!(
                                "{color}{BOLD}{header}{RESET}",
                                color = color_code,
                                BOLD = color::BOLD,
                                RESET = color::RESET
                            );
                        } else {
                            println!("{header}");
                        }
                    }
                    if let Some(save) = self.save.as_mut() {
                        let _ = save.write_line(&header);
                    }
                    if to_stdout {
                        print_hexdump(&b.bytes, color_code, use_color, timestamp, captured_at_ms);
                    }
                    if let Some(save) = self.save.as_mut() {
                        let _ = write_hexdump_to_file(save, &b.bytes, timestamp, captured_at_ms);
                    }
                    self.plain.reset_line();
                    self.save_plain.reset_line();
                }
                DecodedPacket::Other {
                    meta,
                    kind,
                    payload,
                    ..
                } => {
                    eprintln!(
                        "on9log-capture: {kind} packet seq {} t={}ms ({} bytes)",
                        meta.seq,
                        meta.time_ms,
                        payload.len()
                    );
                }
                DecodedPacket::Malformed { meta, reason } => {
                    let seq = meta.as_ref().map(|m| m.seq.to_string()).unwrap_or_default();
                    eprintln!("on9log-capture: malformed packet seq {seq}: {reason}");
                }
            },
            Outcome::BadMagic => {
                eprintln!("on9log-capture: frame with bad magic discarded")
            }
            Outcome::CrcMismatch => eprintln!("on9log-capture: frame failed CRC, discarded"),
            Outcome::Truncated => eprintln!("on9log-capture: truncated frame discarded"),
            Outcome::LengthMismatch => {
                eprintln!("on9log-capture: frame payload length mismatch, discarded")
            }
            Outcome::FrameTooLong => {
                eprintln!("on9log-capture: transport frame exceeded maximum length, discarded")
            }
            Outcome::UnknownFrameType(t) => {
                eprintln!("on9log-capture: unknown transport frame type 0x{t:02x}, discarded");
            }
            Outcome::InvalidEscape => eprintln!("on9log-capture: invalid SLIP escape, discarded"),
            Outcome::PlainText(bytes) => {
                let mut out = std::io::stdout().lock();
                if to_stdout {
                    let _ = write_plain_text(
                        &mut out,
                        bytes,
                        timestamp,
                        captured_at_ms,
                        strip_ansi,
                        &mut self.plain,
                    );
                }
                if let Some(save) = self.save.as_mut() {
                    let _ = write_plain_text_saved(
                        save,
                        bytes,
                        timestamp,
                        captured_at_ms,
                        strip_ansi,
                        &mut self.save_plain,
                    );
                }
                for annotation in self.crash.feed(bytes, elf) {
                    let annotation = clean_string(&annotation, strip_ansi);
                    if to_stdout {
                        let _ = write_plain_annotation(
                            &mut out,
                            &annotation,
                            timestamp,
                            captured_at_ms,
                        );
                    }
                    if let Some(save) = self.save.as_mut() {
                        let _ = save.write_line(&format!(
                            "{}{}",
                            ts_prefix(timestamp, captured_at_ms),
                            annotation
                        ));
                    }
                }
                let _ = out.flush();
            }
        }
    }

    /// Print a dim, yellow warning/progress line, optionally prefixed with
    /// the captured-at timestamp. The message is ANSI-stripped if the renderer
    /// is configured for stripping.
    fn warn_line(&mut self, msg: &str, captured_at_ms: u64) {
        let RenderOptions {
            use_color,
            timestamp,
            to_stdout,
            strip_ansi,
            ..
        } = self.opts;
        let msg = clean_string(
            &format!("{}{}", ts_prefix(timestamp, captured_at_ms), msg),
            strip_ansi,
        );
        if to_stdout {
            if use_color {
                println!(
                    "{DIM}{YELLOW}{msg}{RESET}",
                    DIM = color::DIM,
                    YELLOW = color::YELLOW,
                    RESET = color::RESET
                );
            } else {
                println!("{msg}");
            }
        }
        if let Some(save) = self.save.as_mut() {
            let _ = save.write_line(&msg);
        }
    }
}

/// Map an on9log [`Level`] to the corresponding ANSI color constant.
fn level_color(level: Level) -> &'static str {
    match level {
        Level::Error => color::RED,
        Level::Warn => color::YELLOW,
        Level::Info => color::GREEN,
        _ => color::WHITE,
    }
}

// ---------------------------------------------------------------------------
// Plain text + save file helpers
// ---------------------------------------------------------------------------

/// A file that mirrors decoded text output, with synchronous writes and
/// data-sync after each operation for crash safety.
struct SaveLog {
    /// Path to the log file on disk.
    path: PathBuf,
    /// Open writable file handle.
    file: std::fs::File,
}

impl SaveLog {
    /// Create a new save file at the given `path`. The file is created
    /// (truncating any existing content).
    fn create(path: &std::path::Path) -> std::io::Result<Self> {
        let file = std::fs::File::create(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
    }

    /// Write all `bytes` to the file, flush, and `sync_data()` for durability.
    fn write_all_immediate(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.file.write_all(bytes)?;
        self.file.flush()?;
        self.file.sync_data()
    }

    /// Write a single `line` (with trailing newline), flush, and `sync_data()`.
    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.file.sync_data()
    }
}

/// Display the save-file path for user-facing output.
impl std::fmt::Display for SaveLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path.display())
    }
}

/// Tracks plain-text output position for inserting timestamps at line starts
/// and for stripping ANSI escape sequences in the save-file path.
struct PlainTextState {
    /// Whether the next byte to be written is at the start of a fresh line.
    line_start: bool,
    /// Current state of the streaming ANSI escape sequence parser.
    ansi: AnsiStripState,
}

impl PlainTextState {
    /// Create a new state positioned at the start of a line with the ANSI
    /// parser in the ground state.
    fn new() -> Self {
        Self {
            line_start: true,
            ansi: AnsiStripState::Ground,
        }
    }

    /// Reset to line-start with ground ANSI state.
    fn reset_line(&mut self) {
        self.line_start = true;
        self.ansi = AnsiStripState::Ground;
    }
}

/// State machine for the streaming ANSI escape sequence parser.
///
/// Transitions: `Ground` -> (`0x1b`) -> `Escape` -> (`[`) -> `Csi` -> (final
/// byte 0x40-0x7e) -> `Ground`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AnsiStripState {
    /// Normal character output; no escape sequence in progress.
    Ground,
    /// Saw `0x1b` (ESC); waiting for the next byte to determine the sequence.
    Escape,
    /// Saw `[` after ESC; accumulating CSI parameters until the final byte.
    Csi,
}

/// Return the timestamp prefix string `[YYYYMMDD-HH:MM:SS.mmm] ` if `enabled`
/// is true, otherwise an empty string.
fn ts_prefix(enabled: bool, captured_at_ms: u64) -> String {
    if !enabled {
        return String::new();
    }
    format!("[{}] ", local_ts_string(captured_at_ms))
}

/// Print a hex + ASCII dump of `bytes` to stdout, 16 bytes per line, with
/// optional ANSI color and timestamp prefix.
fn print_hexdump(
    bytes: &[u8],
    color_code: &str,
    use_color: bool,
    timestamp: bool,
    captured_at_ms: u64,
) {
    const BYTES_PER_LINE: usize = 16;
    for (i, chunk) in bytes.chunks(BYTES_PER_LINE).enumerate() {
        let addr = (i * BYTES_PER_LINE) as u32;
        let hex: Vec<String> = chunk.iter().map(|b| format!("{:02x}", b)).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        let line = format!(
            "{}{:08x}  {:<48}  {}",
            ts_prefix(timestamp, captured_at_ms),
            addr,
            hex.join(" "),
            ascii
        );
        if use_color {
            println!(
                "{color}{line}{RESET}",
                color = color_code,
                RESET = color::RESET
            );
        } else {
            println!("{line}");
        }
    }
}

/// Write a hex + ASCII dump of `bytes` to the save file, 16 bytes per line,
/// with optional timestamp prefix.
fn write_hexdump_to_file(
    save: &mut SaveLog,
    bytes: &[u8],
    timestamp: bool,
    captured_at_ms: u64,
) -> std::io::Result<()> {
    const BYTES_PER_LINE: usize = 16;
    for (i, chunk) in bytes.chunks(BYTES_PER_LINE).enumerate() {
        let addr = (i * BYTES_PER_LINE) as u32;
        let hex: Vec<String> = chunk.iter().map(|b| format!("{:02x}", b)).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        save.write_line(&format!(
            "{}{:08x}  {:<48}  {}",
            ts_prefix(timestamp, captured_at_ms),
            addr,
            hex.join(" "),
            ascii
        ))?;
    }
    Ok(())
}

/// Write raw plain-text bytes to the output, optionally inserting a timestamp
/// at each line start and optionally stripping ANSI escape sequences. Tracks
/// the line-start state across calls so chunked input is handled correctly.
fn write_plain_text<W: Write>(
    out: &mut W,
    bytes: &[u8],
    timestamp: bool,
    captured_at_ms: u64,
    strip_ansi: bool,
    state: &mut PlainTextState,
) -> std::io::Result<()> {
    let clean;
    let bytes = if strip_ansi {
        clean = strip_ansi_stream(bytes, &mut state.ansi);
        clean.as_slice()
    } else {
        bytes
    };
    for &b in bytes {
        if timestamp && state.line_start {
            out.write_all(ts_prefix(true, captured_at_ms).as_bytes())?;
            state.line_start = false;
        }
        out.write_all(&[b])?;
        if b == b'\n' {
            state.line_start = true;
        }
    }
    Ok(())
}

/// Write a single annotation line (e.g. a crash backtrace marker) to the
/// output, prefixed with an optional timestamp and always terminated by a
/// newline.
fn write_plain_annotation<W: Write>(
    out: &mut W,
    line: &str,
    timestamp: bool,
    captured_at_ms: u64,
) -> std::io::Result<()> {
    out.write_all(ts_prefix(timestamp, captured_at_ms).as_bytes())?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")
}

/// Write plain-text bytes to the save file, optionally stripping ANSI escape
/// sequences and inserting timestamps at each line start.
fn write_plain_text_saved(
    save: &mut SaveLog,
    bytes: &[u8],
    timestamp: bool,
    captured_at_ms: u64,
    strip_ansi: bool,
    state: &mut PlainTextState,
) -> std::io::Result<()> {
    let clean;
    let bytes = if strip_ansi {
        clean = strip_ansi_stream(bytes, &mut state.ansi);
        clean.as_slice()
    } else {
        bytes
    };
    let mut out = Vec::with_capacity(bytes.len() + 32);
    for &b in bytes {
        if timestamp && state.line_start {
            out.extend_from_slice(ts_prefix(true, captured_at_ms).as_bytes());
            state.line_start = false;
        }
        out.push(b);
        if b == b'\n' {
            state.line_start = true;
        }
    }
    save.write_all_immediate(&out)?;
    Ok(())
}

/// Remove ANSI escape sequences from `bytes` in a single pass.
fn strip_ansi_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut state = AnsiStripState::Ground;
    strip_ansi_stream(bytes, &mut state)
}

/// Conditionally strip ANSI escape sequences from a string. Returns the
/// original string (cloned) when `strip_ansi` is false.
fn clean_string(s: &str, strip_ansi: bool) -> String {
    if !strip_ansi {
        return s.to_string();
    }
    String::from_utf8_lossy(&strip_ansi_bytes(s.as_bytes())).into_owned()
}

/// Streaming ANSI escape sequence remover. `state` tracks whether we are
/// inside an escape sequence across calls so that split sequences (spanning
/// multiple chunks) are handled correctly.
fn strip_ansi_stream(bytes: &[u8], state: &mut AnsiStripState) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        match *state {
            AnsiStripState::Ground => {
                if b == 0x1b {
                    *state = AnsiStripState::Escape;
                } else {
                    out.push(b);
                }
            }
            AnsiStripState::Escape => {
                if b == b'[' {
                    *state = AnsiStripState::Csi;
                } else {
                    *state = AnsiStripState::Ground;
                }
            }
            AnsiStripState::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    *state = AnsiStripState::Ground;
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// ESP reset
// ---------------------------------------------------------------------------

/// Perform a hard reset of an ESP target by toggling the serial port's RTS
/// line (connected to EN on ESP dev boards via an active-low transistor
/// circuit). DTR is held high (GPIO0 released, normal boot).
fn esp_hard_reset<P: SerialPort>(port: &mut P) -> tokio_serial::Result<()> {
    // ESP dev boards wire DTR to GPIO0 and RTS to EN through active-low
    // transistor circuits. Release GPIO0, pulse EN low, then release EN.
    port.write_data_terminal_ready(false)?;
    port.write_request_to_send(true)?;
    std::thread::sleep(Duration::from_millis(100));
    port.write_request_to_send(false)?;
    std::thread::sleep(Duration::from_millis(100));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_each_plain_text_line() {
        let mut out = Vec::new();
        let mut state = PlainTextState::new();
        write_plain_text(
            &mut out,
            b"a\nb",
            true,
            1_700_000_000_000,
            false,
            &mut state,
        )
        .unwrap();
        let s = String::from_utf8(out).unwrap();

        assert_eq!(s.matches('[').count(), 2);
        assert!(s.ends_with("b"));
        assert!(!state.line_start);
    }

    #[test]
    fn plain_text_timestamp_state_spans_chunks() {
        let mut out = Vec::new();
        let mut state = PlainTextState::new();
        write_plain_text(&mut out, b"a", true, 1_700_000_000_000, false, &mut state).unwrap();
        write_plain_text(&mut out, b"b\n", true, 1_700_000_000_500, false, &mut state).unwrap();
        let s = String::from_utf8(out).unwrap();

        assert_eq!(s.matches('[').count(), 1);
        assert!(state.line_start);
    }

    #[test]
    fn preserves_device_ansi_plain_text_colors() {
        let mut out = Vec::new();
        let mut state = PlainTextState::new();
        write_plain_text(
            &mut out,
            b"\x1b[0;32mI (1519) demo: heartbeat 2\x1b[0m\n",
            false,
            0,
            false,
            &mut state,
        )
        .unwrap();

        assert_eq!(out, b"\x1b[0;32mI (1519) demo: heartbeat 2\x1b[0m\n");
    }

    #[test]
    fn strips_device_ansi_plain_text_when_requested() {
        let mut out = Vec::new();
        let mut state = PlainTextState::new();
        write_plain_text(
            &mut out,
            b"\x1b[0;32mI (1519) demo: heartbeat 2\x1b[0m\n",
            false,
            0,
            true,
            &mut state,
        )
        .unwrap();

        assert_eq!(out, b"I (1519) demo: heartbeat 2\n");
    }

    #[test]
    fn strip_ansi_removes_color_sequences() {
        assert_eq!(
            strip_ansi_bytes(b"\x1b[0;32mI (1) tag: colored\x1b[0m\n"),
            b"I (1) tag: colored\n"
        );
    }

    #[test]
    fn strip_ansi_stream_handles_split_sequences() {
        let mut state = AnsiStripState::Ground;
        let mut out = strip_ansi_stream(b"\x1b[0;", &mut state);
        assert_eq!(out, b"");
        out.extend(strip_ansi_stream(b"32mgreen\x1b", &mut state));
        out.extend(strip_ansi_stream(b"[0m\n", &mut state));
        assert_eq!(out, b"green\n");
        assert_eq!(state, AnsiStripState::Ground);
    }

    #[test]
    fn default_capture_path_shape() {
        let path = default_capture_path();
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("on9log-capture-"));
        assert!(name.ends_with(".sqlite"));
        assert!(
            name["on9log-capture-".len()..name.len() - ".sqlite".len()]
                .chars()
                .all(|c| c.is_ascii_digit())
        );
    }
}
