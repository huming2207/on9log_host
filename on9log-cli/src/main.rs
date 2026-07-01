//! `on9log` — host-side CLI for the on9log binary log stream.
//!
//! Reads a UART, captured binary stream file, or stdin; deframes typed SLIP+CRC
//! transport frames; decodes on9log packets against an optional ELF; and
//! prints colorized log lines.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::ptr;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{ArgGroup, Parser};
use tokio::io::AsyncReadExt;
use tokio_serial::{SerialPort, SerialPortBuilderExt};

use on9log_protocol::{CrashDecoder, DecodedPacket, Decoder, Deframer, ElfStrings, Level, Outcome};

mod term;

use term::color;

/// Host-side decoder for on9log binary log streams.
#[derive(Parser, Debug)]
#[command(
    name = "on9log",
    version,
    about,
    group(
        ArgGroup::new("input")
            .required(true)
            .args(["port", "log_bin", "log_stdin"])
    )
)]
struct Cli {
    /// Read from a UART device, e.g. /dev/ttyUSB0.
    #[arg(short, long, value_name = "PORT")]
    port: Option<String>,

    /// Replay a binary log stream previously captured from stdout.
    #[arg(long, value_name = "FILE")]
    log_bin: Option<PathBuf>,

    /// Read a live binary log stream from stdin.
    #[arg(long)]
    log_stdin: bool,

    /// Baud rate.
    #[arg(short, long, value_name = "BAUD", default_value_t = 115_200)]
    baud: u32,

    /// Matching ELF or Mach-O image used to resolve format/tag addresses.
    #[arg(long, value_name = "FILE")]
    elf: Option<PathBuf>,

    /// Disable colored output.
    #[arg(long)]
    no_color: bool,

    /// Prefix each decoded log and each plain-text line with local wall time.
    #[arg(short = 't', long)]
    timestamp: bool,

    /// Override detected terminal width (0 = auto-detect).
    #[arg(long, default_value_t = 0)]
    width: usize,

    /// Do not reset ESP targets by toggling DTR/RTS when the port opens.
    #[arg(long)]
    no_esp_reset: bool,

    /// Save decoded human-readable log output to a text file.
    #[arg(short = 's', long)]
    save: bool,

    /// Path for --save output; defaults to on9log-UNIX_TIMESTAMP.log.
    #[arg(long, value_name = "FILE", requires = "save")]
    log_path: Option<PathBuf>,
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let use_color = !cli.no_color && term::stdout_is_tty();
    let width = if cli.width > 0 {
        cli.width
    } else {
        term::terminal_width()
    };

    let elf = match &cli.elf {
        Some(p) => match ElfStrings::from_path(p) {
            Ok(e) => {
                eprintln!("on9log: loaded image {}", p.display());
                Some(Rc::new(e))
            }
            Err(e) => {
                eprintln!("on9log: failed to parse image {}: {e}", p.display());
                None
            }
        },
        None => {
            eprintln!("on9log: no --elf given; format/tag addresses will render as hex");
            None
        }
    };
    let save = if cli.save {
        Some(SaveLog::create(cli.log_path.clone())?)
    } else {
        None
    };

    if let Some(port) = cli.port {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(run(RunConfig {
            port,
            baud: cli.baud,
            elf,
            use_color,
            width,
            timestamp: cli.timestamp,
            esp_reset: !cli.no_esp_reset,
            save,
        }))?;
    } else if let Some(path) = cli.log_bin {
        let file =
            File::open(&path).map_err(|e| format!("opening binary log {}: {e}", path.display()))?;
        run_reader(
            ReplayConfig {
                elf,
                use_color,
                width,
                timestamp: cli.timestamp,
                save,
            },
            file,
            &format!("binary log {}", path.display()),
        )?;
    } else if cli.log_stdin {
        let stdin = std::io::stdin();
        run_reader(
            ReplayConfig {
                elf,
                use_color,
                width,
                timestamp: cli.timestamp,
                save,
            },
            stdin.lock(),
            "stdin",
        )?;
    } else {
        unreachable!("clap requires exactly one input source");
    }
    Ok(())
}

/// Runtime configuration assembled from [`Cli`] arguments before entering the
/// async event loop.
struct RunConfig {
    /// UART port device path, e.g. `/dev/ttyUSB0`.
    port: String,
    /// Baud rate for the serial connection.
    baud: u32,
    /// Optional ELF strings table for resolving format/tag addresses.
    elf: Option<Rc<ElfStrings>>,
    /// Whether to emit ANSI color codes.
    use_color: bool,
    /// Terminal width for line wrapping (columns).
    width: usize,
    /// Whether to prefix each output line with a local timestamp.
    timestamp: bool,
    /// Whether to toggle DTR/RTS to reset the ESP target on open.
    esp_reset: bool,
    /// Optional text-file logger for saving decoded output.
    save: Option<SaveLog>,
}

/// Configuration used only by file and stdin replay. Serial-specific settings
/// intentionally remain in [`RunConfig`] and the original UART event loop.
struct ReplayConfig {
    elf: Option<Rc<ElfStrings>>,
    use_color: bool,
    width: usize,
    timestamp: bool,
    save: Option<SaveLog>,
}

const IMAGE_SLIDE_PREFIX: &[u8] = b"@on9log-image-slide=";

/// Startup-only parser for the macOS host demo's image-slide metadata line.
/// Non-matching input is passed through unchanged to the normal deframer.
struct ReplayPreamble {
    pending: Vec<u8>,
    decided: bool,
}

enum ReplayPreambleResult {
    Pending,
    Data(Vec<u8>),
    ImageSlide(u32, Vec<u8>),
}

impl ReplayPreamble {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            decided: false,
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> ReplayPreambleResult {
        self.pending.extend_from_slice(bytes);

        if self.pending.len() <= IMAGE_SLIDE_PREFIX.len()
            && IMAGE_SLIDE_PREFIX.starts_with(&self.pending)
        {
            return ReplayPreambleResult::Pending;
        }

        if self.pending.starts_with(IMAGE_SLIDE_PREFIX) {
            if let Some(newline) = self.pending.iter().position(|&b| b == b'\n') {
                let value = &self.pending[IMAGE_SLIDE_PREFIX.len()..newline];
                if let Ok(value) = std::str::from_utf8(value)
                    && let Ok(slide) = u32::from_str_radix(value, 16)
                {
                    let remaining = self.pending.split_off(newline + 1);
                    self.pending.clear();
                    self.decided = true;
                    return ReplayPreambleResult::ImageSlide(slide, remaining);
                }
            } else if self.pending.len() < 128 {
                return ReplayPreambleResult::Pending;
            }
        }

        self.decided = true;
        ReplayPreambleResult::Data(std::mem::take(&mut self.pending))
    }

    fn finish(&mut self) -> Option<Vec<u8>> {
        if self.pending.is_empty() {
            None
        } else {
            self.decided = true;
            Some(std::mem::take(&mut self.pending))
        }
    }
}

/// Main async event loop: opens the serial port, optionally resets the ESP
/// target, then reads serial bytes through the deframer/decode pipeline
/// until EOF or the user presses the quit key sequence (`Ctrl+] Ctrl+T`).
///
/// # Errors
/// Returns an error if the serial port cannot be opened, the ESP reset fails,
/// or an unrecoverable I/O error occurs during the read loop.
async fn run(mut cfg: RunConfig) -> Result<(), Box<dyn std::error::Error>> {
    let port = cfg.port;
    let baud = cfg.baud;
    let mut serial = tokio_serial::new(&port, baud)
        .open_native_async()
        .map_err(|e| format!("opening {port}: {e}"))?;

    if cfg.esp_reset {
        esp_hard_reset(&mut serial).map_err(|e| format!("resetting ESP target on {port}: {e}"))?;
    }

    let mut deframer = Deframer::new();
    let mut state = DecodeState::new();
    let mut buf = vec![0u8; 4096];
    let mut stdin_buf = [0u8; 64];
    let mut monitor_keys = MonitorKeyState::new();
    let mut stdin = tokio::io::stdin();
    let raw_input = RawInputGuard::enter_if_interactive();

    eprintln!(
        "on9log: listening on {port} @ {baud} baud (width {width}, esp-reset {}, quit Ctrl+] Ctrl+T)",
        if cfg.esp_reset { "on" } else { "off" },
        width = cfg.width
    );
    if let Some(save) = cfg.save.as_ref() {
        eprintln!("on9log: saving decoded log to {}", save.path.display());
    }

    let opts = RenderOptions {
        use_color: cfg.use_color,
        width: cfg.width,
        timestamp: cfg.timestamp,
    };

    loop {
        let n = if raw_input.is_active() {
            tokio::select! {
                serial_read = serial.read(&mut buf) => serial_read?,
                stdin_read = stdin.read(&mut stdin_buf) => {
                    let n = stdin_read?;
                    if n == 0 {
                        continue;
                    }
                    if monitor_keys.feed(&stdin_buf[..n]) {
                        eprintln!("on9log: quit requested");
                        break;
                    }
                    continue;
                }
            }
        } else {
            serial.read(&mut buf).await?
        };
        if n == 0 {
            // EOF on the device; stop.
            break;
        }
        for outcome in deframer.feed(&buf[..n]) {
            handle_outcome(
                outcome,
                &mut state,
                cfg.elf.as_deref(),
                opts,
                cfg.save.as_mut(),
            );
        }
    }

    Ok(())
}

/// Read a finite or piped byte stream and feed it through the same deframer and
/// renderer used by the serial monitor.
fn run_reader<R: std::io::Read>(
    mut cfg: ReplayConfig,
    mut reader: R,
    source: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut deframer = Deframer::new();
    let mut state = DecodeState::new();
    let mut buf = [0u8; 4096];
    let mut preamble = ReplayPreamble::new();
    let opts = RenderOptions {
        use_color: cfg.use_color,
        width: cfg.width,
        timestamp: cfg.timestamp,
    };

    eprintln!("on9log: reading from {source} (width {})", cfg.width);
    if let Some(save) = cfg.save.as_ref() {
        eprintln!("on9log: saving decoded log to {}", save.path.display());
    }

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if preamble.decided {
            process_input_bytes(
                &buf[..n],
                &mut deframer,
                &mut state,
                cfg.elf.as_deref(),
                opts,
                cfg.save.as_mut(),
            );
            continue;
        }

        match preamble.feed(&buf[..n]) {
            ReplayPreambleResult::Pending => {}
            ReplayPreambleResult::Data(data) => process_input_bytes(
                &data,
                &mut deframer,
                &mut state,
                cfg.elf.as_deref(),
                opts,
                cfg.save.as_mut(),
            ),
            ReplayPreambleResult::ImageSlide(slide, data) => {
                if let Some(elf) = cfg.elf.as_deref() {
                    elf.set_address_slide(slide);
                    eprintln!("on9log: applied host image slide 0x{slide:08x}");
                }
                process_input_bytes(
                    &data,
                    &mut deframer,
                    &mut state,
                    cfg.elf.as_deref(),
                    opts,
                    cfg.save.as_mut(),
                );
            }
        }
    }

    if let Some(data) = preamble.finish() {
        process_input_bytes(
            &data,
            &mut deframer,
            &mut state,
            cfg.elf.as_deref(),
            opts,
            cfg.save.as_mut(),
        );
    }

    Ok(())
}

/// Feed one arbitrary input chunk through the shared deframer/decode path.
fn process_input_bytes(
    bytes: &[u8],
    deframer: &mut Deframer,
    state: &mut DecodeState,
    elf: Option<&ElfStrings>,
    opts: RenderOptions,
    mut save: Option<&mut SaveLog>,
) {
    for outcome in deframer.feed(bytes) {
        handle_outcome(outcome, state, elf, opts, save.as_deref_mut());
    }
}

/// A text log file that mirrors decoded output, with synchronous writes and
/// data-sync after each operation so the file is never truncated on crash.
struct SaveLog {
    /// Path to the log file on disk.
    path: PathBuf,
    /// Open writable file handle.
    file: std::fs::File,
}

impl SaveLog {
    /// Create a new save file. If `path` is `None`, a default
    /// `on9log-UNIX_TIMESTAMP.log` path is generated.
    fn create(path: Option<PathBuf>) -> std::io::Result<Self> {
        let path = path.unwrap_or_else(default_save_path);
        let file = std::fs::File::create(&path)?;
        Ok(Self { path, file })
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

/// Generate a default save path: `on9log-{unix_epoch_seconds}.log`.
fn default_save_path() -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    PathBuf::from(format!("on9log-{ts}.log"))
}

/// RAII guard that places the terminal in raw (non-canonical) input mode so
/// that quit-key sequences (`Ctrl+] Ctrl+T`) are received immediately. The
/// original termios settings are restored on drop.
struct RawInputGuard {
    /// Whether raw mode is currently active.
    active: bool,
    /// File descriptor of stdin (used for `tcsetattr` on drop).
    #[cfg(unix)]
    fd: std::os::fd::RawFd,
    /// Original termios settings to restore on drop.
    #[cfg(unix)]
    original: libc::termios,
}

impl RawInputGuard {
    /// Enter raw input mode if stdin is a TTY. Returns an inactive guard
    /// (no-op on drop) if stdin is not interactive or if mode switching fails.
    fn enter_if_interactive() -> Self {
        if !term::stdin_is_tty() {
            return Self::inactive();
        }
        match enable_raw_input_mode() {
            Ok(guard) => guard,
            Err(e) => {
                eprintln!("on9log: failed to enable terminal control keys: {e}");
                Self::inactive()
            }
        }
    }

    /// Create an inactive guard that performs no terminal manipulation on drop.
    fn inactive() -> Self {
        Self {
            active: false,
            #[cfg(unix)]
            fd: -1,
            #[cfg(unix)]
            original: unsafe { std::mem::zeroed() },
        }
    }

    /// Returns `true` if raw input mode was successfully enabled.
    fn is_active(&self) -> bool {
        self.active
    }
}

impl Drop for RawInputGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.active {
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
            }
        }
        #[cfg(not(unix))]
        if self.active {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// Enable Unix raw (non-canonical) terminal mode on stdin so that control
/// characters such as `Ctrl+]` (0x1d) arrive immediately without line
/// buffering. Returns an RAII guard that restores the original settings.
#[cfg(unix)]
fn enable_raw_input_mode() -> std::io::Result<RawInputGuard> {
    use std::os::fd::AsRawFd;

    let fd = std::io::stdin().as_raw_fd();
    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let original = unsafe { original.assume_init() };
    let mut raw = original;

    // Non-canonical input lets Ctrl+] Ctrl+T arrive immediately. Keep output
    // processing intact so monitor newlines are not affected.
    raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;

    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(RawInputGuard {
        active: true,
        fd,
        original,
    })
}

#[cfg(not(unix))]
fn enable_raw_input_mode() -> std::io::Result<RawInputGuard> {
    crossterm::terminal::enable_raw_mode()?;
    Ok(RawInputGuard { active: true })
}

/// Tracks whether the user has typed the quit sequence `Ctrl+] Ctrl+T`.
/// The sequence can span multiple `feed()` calls (partial chunks).
struct MonitorKeyState {
    /// Whether `Ctrl+]` (0x1d) was seen as the most recent key.
    saw_ctrl_rbracket: bool,
}

impl MonitorKeyState {
    /// Create a new idle key state.
    fn new() -> Self {
        Self {
            saw_ctrl_rbracket: false,
        }
    }

    /// Feed a chunk of stdin bytes and return `true` if the complete quit
    /// sequence (`Ctrl+] Ctrl+T`) has been detected. The sequence can span
    /// multiple calls; any byte that does not continue the sequence resets.
    fn feed(&mut self, bytes: &[u8]) -> bool {
        for &b in bytes {
            match (self.saw_ctrl_rbracket, b) {
                (true, CTRL_T) => return true,
                (_, CTRL_RBRACKET) => self.saw_ctrl_rbracket = true,
                _ => self.saw_ctrl_rbracket = false,
            }
        }
        false
    }
}

/// `Ctrl+]` byte (ASCII group separator, 0x1d). Used as the first key in the
/// quit sequence.
const CTRL_RBRACKET: u8 = 0x1d;
/// `Ctrl+T` byte (ASCII end of transmission, 0x14). Used as the second key in
/// the quit sequence.
const CTRL_T: u8 = 0x14;

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

/// Rendering configuration passed to `handle_outcome()`.
#[derive(Clone, Copy)]
struct RenderOptions {
    /// Whether to emit ANSI color codes.
    use_color: bool,
    /// Terminal width for line wrapping (columns). 0 disables wrapping.
    width: usize,
    /// Whether to prefix each line with a local timestamp.
    timestamp: bool,
}

/// Persistent state for the on9log decode pipeline, wrapping the protocol
/// [`Decoder`], [`CrashDecoder`], and plain-text tracking state.
struct DecodeState {
    /// Decoder for on9log binary frames.
    decoder: Decoder,
    /// State for tracking plain-text line starts (used for timestamp insertion
    /// on stdout).
    plain_text: PlainTextState,
    /// Crash-decoder for extracting backtrace annotations from plain text.
    crash_decoder: CrashDecoder,
    /// Separate plain-text state for the save-file path (tracks ANSI stripping
    /// independently).
    save_plain_text: PlainTextState,
}

impl DecodeState {
    /// Create a new decode state with fresh decoders and reset plain-text tracking.
    fn new() -> Self {
        Self {
            decoder: Decoder::new(),
            plain_text: PlainTextState::new(),
            crash_decoder: CrashDecoder::new(),
            save_plain_text: PlainTextState::new(),
        }
    }
}

/// Decode a single transport [`Outcome`] and render it to stdout (and
/// optionally to the save file). Handles both on9log frames (via the
/// [`Decoder`]) and plain-text / diagnostic outcomes.
fn handle_outcome(
    outcome: Outcome,
    state: &mut DecodeState,
    elf: Option<&ElfStrings>,
    opts: RenderOptions,
    mut save: Option<&mut SaveLog>,
) {
    let RenderOptions {
        use_color,
        width,
        timestamp,
    } = opts;
    match outcome {
        Outcome::Frame(frame) => match state.decoder.decode(&frame, elf) {
            DecodedPacket::Log(l) => {
                let color_code = level_color(l.level);
                let prefix = format!(
                    "{}{} ({:>7}) {}: ",
                    timestamp_prefix(timestamp),
                    l.level.letter(),
                    l.meta.time_ms,
                    l.tag
                );
                let indent = prefix.chars().count();
                if let Some(gap) = l.meta.gap {
                    let msg = format!("--- missed {gap} packet(s) before seq {} ---", l.meta.seq);
                    warn_line(&msg, use_color, timestamp);
                    if let Some(save) = save.as_deref_mut() {
                        let _ = save.write_line(&format!("{}{}", timestamp_prefix(timestamp), msg));
                    }
                }
                term::print_log_line(&prefix, &l.message, color_code, indent, width, use_color);
                if let Some(save) = save.as_deref_mut() {
                    let _ = save.write_line(&format!("{prefix}{}", l.message));
                }
                state.plain_text.reset_line();
                state.save_plain_text.reset_line();
            }
            DecodedPacket::Dropped(d) => {
                let msg = format!(
                    "--- device dropped {} packet(s) at seq {} (t={}ms) ---",
                    d.count, d.meta.seq, d.meta.time_ms
                );
                warn_line(&msg, use_color, timestamp);
                if let Some(save) = save.as_deref_mut() {
                    let _ = save.write_line(&format!("{}{}", timestamp_prefix(timestamp), msg));
                }
                state.plain_text.reset_line();
                state.save_plain_text.reset_line();
            }
            DecodedPacket::Buffer(b) => {
                let color_code = level_color(b.level);
                let header = format!(
                    "{}{} ({:>7}) {} [buf {} @{} +{}]: <buffer dump, {} bytes>",
                    timestamp_prefix(timestamp),
                    b.level.letter(),
                    b.meta.time_ms,
                    b.tag,
                    b.total_len,
                    b.offset,
                    b.bytes.len(),
                    b.bytes.len()
                );
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
                if let Some(save) = save.as_deref_mut() {
                    let _ = save.write_line(&header);
                }
                print_hexdump(&b.bytes, color_code, use_color, timestamp);
                if let Some(save) = save.as_deref_mut() {
                    let _ = write_hexdump_to_file(save, &b.bytes, timestamp);
                }
                state.plain_text.reset_line();
                state.save_plain_text.reset_line();
            }
            DecodedPacket::Other {
                meta,
                kind,
                payload,
                ..
            } => {
                eprintln!(
                    "on9log: {kind} packet seq {} t={}ms ({} bytes)",
                    meta.seq,
                    meta.time_ms,
                    payload.len()
                );
            }
            DecodedPacket::Malformed { meta, reason } => {
                let seq = meta.as_ref().map(|m| m.seq.to_string()).unwrap_or_default();
                eprintln!("on9log: malformed packet seq {seq}: {reason}");
            }
        },
        Outcome::BadMagic => eprintln!("on9log: frame with bad magic discarded"),
        Outcome::CrcMismatch => eprintln!("on9log: frame failed CRC, discarded"),
        Outcome::Truncated => eprintln!("on9log: truncated frame discarded"),
        Outcome::LengthMismatch => eprintln!("on9log: frame payload length mismatch, discarded"),
        Outcome::FrameTooLong => {
            eprintln!("on9log: transport frame exceeded maximum length, discarded")
        }
        Outcome::UnknownFrameType(t) => {
            eprintln!("on9log: unknown transport frame type 0x{t:02x}, discarded");
        }
        Outcome::InvalidEscape => eprintln!("on9log: invalid SLIP escape, discarded"),
        Outcome::PlainText(bytes) => {
            let mut out = std::io::stdout().lock();
            let _ = write_plain_text(&mut out, &bytes, timestamp, &mut state.plain_text);
            if let Some(save) = save.as_deref_mut() {
                let _ = write_plain_text_saved(save, &bytes, timestamp, &mut state.save_plain_text);
            }
            for annotation in state.crash_decoder.feed(&bytes, elf) {
                let _ = write_plain_annotation(&mut out, &annotation, timestamp);
                if let Some(save) = save.as_deref_mut() {
                    let _ =
                        save.write_line(&format!("{}{}", timestamp_prefix(timestamp), annotation));
                }
            }
            let _ = out.flush();
        }
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

    /// Reset to line-start with ground ANSI state (called after a formatted
    /// on9log packet is rendered).
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

/// Print a dim, yellow warning/progress line to stdout (with optional
/// timestamp and ANSI coloring).
fn warn_line(msg: &str, use_color: bool, timestamp: bool) {
    let msg = format!("{}{}", timestamp_prefix(timestamp), msg);
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

/// Print a hex + ASCII dump of `bytes` to stdout, 16 bytes per line, with
/// optional ANSI color and timestamp prefix.
fn print_hexdump(bytes: &[u8], color_code: &str, use_color: bool, timestamp: bool) {
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
            timestamp_prefix(timestamp),
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
fn write_hexdump_to_file(save: &mut SaveLog, bytes: &[u8], timestamp: bool) -> std::io::Result<()> {
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
            timestamp_prefix(timestamp),
            addr,
            hex.join(" "),
            ascii
        ))?;
    }
    Ok(())
}

/// Write raw plain-text bytes to the output, optionally inserting a timestamp
/// at each line start. Tracks the line-start state across calls so that
/// chunked input (e.g. from fragmented SLIP frames) is timestamped correctly.
fn write_plain_text<W: Write>(
    out: &mut W,
    bytes: &[u8],
    timestamp: bool,
    state: &mut PlainTextState,
) -> std::io::Result<()> {
    for &b in bytes {
        if timestamp && state.line_start {
            out.write_all(timestamp_prefix(true).as_bytes())?;
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
) -> std::io::Result<()> {
    out.write_all(timestamp_prefix(timestamp).as_bytes())?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")
}

/// Write plain-text bytes to the save file, stripping ANSI escape sequences
/// and optionally inserting timestamps at each line start.
fn write_plain_text_saved(
    save: &mut SaveLog,
    bytes: &[u8],
    timestamp: bool,
    state: &mut PlainTextState,
) -> std::io::Result<()> {
    let clean = strip_ansi_stream(bytes, &mut state.ansi);
    let mut out = Vec::with_capacity(clean.len() + 32);
    for &b in &clean {
        if timestamp && state.line_start {
            out.extend_from_slice(timestamp_prefix(true).as_bytes());
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

/// Remove ANSI escape sequences from `bytes` in a single pass (test helper).
#[cfg(test)]
fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut state = AnsiStripState::Ground;
    strip_ansi_stream(bytes, &mut state)
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

/// Return the timestamp prefix string `[YYYYMMDD-HH:MM:SS.mmm] ` if `enabled`
/// is true, otherwise an empty string.
fn timestamp_prefix(enabled: bool) -> String {
    if !enabled {
        return String::new();
    }
    format!("[{}] ", local_timestamp())
}

/// Format the current local wall-clock time as `YYYYMMDD-HH:MM:SS.mmm` using
/// `localtime_r` (Unix variant).
#[cfg(unix)]
fn local_timestamp() -> String {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    unsafe {
        libc::gettimeofday(&mut tv, ptr::null_mut());
    }

    let secs: libc::time_t = tv.tv_sec;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    let tm = unsafe {
        if libc::localtime_r(&secs, tm.as_mut_ptr()).is_null() {
            return "19700101-00:00:00.000".to_string();
        }
        tm.assume_init()
    };
    let millis = tv.tv_usec / 1000;

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

/// Fallback timestamp formatter for non-Unix platforms. Returns a fixed
/// placeholder because `gettimeofday` and `localtime_r` are not available.
#[cfg(not(unix))]
fn local_timestamp() -> String {
    "19700101-00:00:00.000".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_accepts_binary_log_file_input() {
        let cli = Cli::try_parse_from(["on9log", "--log-bin", "capture.bin"]).unwrap();
        assert_eq!(cli.log_bin, Some(PathBuf::from("capture.bin")));
        assert!(cli.port.is_none());
        assert!(!cli.log_stdin);
    }

    #[test]
    fn cli_preserves_serial_port_and_baud_input() {
        let cli = Cli::try_parse_from([
            "on9log",
            "--port",
            "/dev/tty.usbserial-test",
            "--baud",
            "921600",
            "--no-esp-reset",
        ])
        .unwrap();
        assert_eq!(cli.port.as_deref(), Some("/dev/tty.usbserial-test"));
        assert_eq!(cli.baud, 921_600);
        assert!(cli.no_esp_reset);
        assert!(cli.log_bin.is_none());
        assert!(!cli.log_stdin);
    }

    #[test]
    fn cli_accepts_stdin_input() {
        let cli = Cli::try_parse_from(["on9log", "--log-stdin"]).unwrap();
        assert!(cli.log_stdin);
        assert!(cli.port.is_none());
        assert!(cli.log_bin.is_none());
    }

    #[test]
    fn cli_requires_exactly_one_input() {
        assert!(Cli::try_parse_from(["on9log"]).is_err());
        assert!(Cli::try_parse_from(["on9log", "--port", "/dev/ttyUSB0", "--log-stdin",]).is_err());
    }

    #[test]
    fn replay_preamble_parses_split_image_slide() {
        let mut preamble = ReplayPreamble::new();
        assert!(matches!(
            preamble.feed(b"@on9log-image-"),
            ReplayPreambleResult::Pending
        ));
        match preamble.feed(b"slide=0123abcd\n\xa5rest") {
            ReplayPreambleResult::ImageSlide(slide, data) => {
                assert_eq!(slide, 0x0123_abcd);
                assert_eq!(data, b"\xa5rest");
            }
            _ => panic!("expected image-slide metadata"),
        }
    }

    #[test]
    fn replay_preamble_preserves_normal_binary_input() {
        let mut preamble = ReplayPreamble::new();
        match preamble.feed(b"\xa5\x01binary") {
            ReplayPreambleResult::Data(data) => assert_eq!(data, b"\xa5\x01binary"),
            _ => panic!("expected normal input passthrough"),
        }
    }

    #[test]
    fn timestamps_each_plain_text_line() {
        let mut out = Vec::new();
        let mut state = PlainTextState::new();
        write_plain_text(&mut out, b"a\nb", true, &mut state).unwrap();
        let s = String::from_utf8(out).unwrap();

        assert_eq!(s.matches('[').count(), 2);
        assert!(s.ends_with("b"));
        assert!(!state.line_start);
    }

    #[test]
    fn plain_text_timestamp_state_spans_chunks() {
        let mut out = Vec::new();
        let mut state = PlainTextState::new();
        write_plain_text(&mut out, b"a", true, &mut state).unwrap();
        write_plain_text(&mut out, b"b\n", true, &mut state).unwrap();
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
            b"\x1b[0;32mI (1519) demo: esp_log plain text main heartbeat 2\x1b[0m\n",
            false,
            &mut state,
        )
        .unwrap();

        assert_eq!(
            out,
            b"\x1b[0;32mI (1519) demo: esp_log plain text main heartbeat 2\x1b[0m\n"
        );
    }

    #[test]
    fn does_not_infer_plain_text_color_from_prefix() {
        let mut out = Vec::new();
        let mut state = PlainTextState::new();
        write_plain_text(&mut out, b"I (1519) demo: uncolored\n", false, &mut state).unwrap();

        assert_eq!(out, b"I (1519) demo: uncolored\n");
    }

    #[test]
    fn plain_annotation_honors_timestamp_flag() {
        let mut out = Vec::new();
        write_plain_annotation(&mut out, "--- 0x42000000: app_main", false).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "--- 0x42000000: app_main\n"
        );
    }

    #[test]
    fn default_save_path_uses_unix_timestamp_shape() {
        let path = default_save_path();
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("on9log-"));
        assert!(name.ends_with(".log"));
        assert!(
            name["on9log-".len()..name.len() - ".log".len()]
                .chars()
                .all(|c| c.is_ascii_digit())
        );
    }

    #[test]
    fn strip_ansi_removes_color_sequences() {
        assert_eq!(
            strip_ansi(b"\x1b[0;32mI (1) tag: colored\x1b[0m\n"),
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
    fn monitor_keys_quit_on_ctrl_rbracket_then_ctrl_t() {
        let mut keys = MonitorKeyState::new();
        assert!(!keys.feed(&[CTRL_RBRACKET]));
        assert!(keys.feed(&[CTRL_T]));
    }

    #[test]
    fn monitor_keys_quit_sequence_can_span_chunks() {
        let mut keys = MonitorKeyState::new();
        assert!(!keys.feed(b"abc"));
        assert!(!keys.feed(&[CTRL_RBRACKET]));
        assert!(keys.feed(&[CTRL_T]));
    }

    #[test]
    fn monitor_keys_reset_on_other_byte() {
        let mut keys = MonitorKeyState::new();
        assert!(!keys.feed(&[CTRL_RBRACKET, b'x', CTRL_T]));
        assert!(!keys.feed(&[CTRL_T]));
    }
}
