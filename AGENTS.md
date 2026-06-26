# on9log host_cli Notes

This directory is the host-side Rust workspace for the `on9log` binary log
stream produced by the C component in the parent directory. It is split into a
protocol library crate and a CLI binary crate:

- `on9log_protocol` implements the decoding pipeline, crash text recognizer,
  printf rendering, and ELF/DWARF resolution with no async runtime dependency,
  so it can be reused by a future `napi-rs` binding for programmatic access;
- `on9log_cli` builds the `on9log` binary, opens a UART port, handles command
  line parsing/reset/timestamps, and prints colorized terminal output.

See the parent `../AGENTS.md` for the firmware-side packet format, SLIP framing,
CRC, and ELF string strategy. This document only describes the host side.

## Goal

Given a UART byte stream emitted by `esp_stdio_log_vfs.c` /
`on9log_esp_vfs.c`, recover individual typed transport frames, verify them,
decode on9log binary packets, resolve format/tag strings from the matching
firmware ELF, and render human-readable colored log output that wraps to the
current terminal width.

The decoder must also be usable as a library (e.g. from a Node.js binding via
`napi-rs`), so the protocol crate is kept free of any I/O / async runtime
dependency. Only the CLI crate pulls in `tokio` (the runtime `tokio-serial`
requires).

## Wire Format Recap (host view)

All multi-byte fields are little-endian. The packet header is **18 bytes**
(`on9log_packet_header_t` in `on9log_fmt.h`):

```text
magic        u8   0x9a
type_level   u8   high nibble: packet type, low nibble: on9log_level_t
seq          u16  wraps naturally
time_ms      u32  milliseconds since boot, wraps naturally
tag_id       u32  tag string address in ELF
fmt_id       u32  format string address in ELF
payload_len  u16  bytes after header, or 0xffff for streaming
```

Packet types (`ON9LOG_PKT_*`):

```text
0  LOG
1  DROPPED
2  TIME_SYNC
3  BOOT
4  BUFFER
```

Log levels (`on9log_level_t`):

```text
0 NONE    1 ERROR    2 WARN    3 INFO    4 DEBUG    5 VERBOSE
```

The ESP VFS transport frames both on9log binary packets and plain-text
stdout/stderr bytes over raw UART with an explicit-start typed SLIP envelope and
a trailing CRC-16-CCITT:

```text
0xa5
SLIP(frame_type)
SLIP(payload bytes)
SLIP(crc16_ccitt_le)
0xc0
```

Transport frame types:

```text
0x01  on9log binary packet (payload is the 18-byte on9log header + payload)
0x02  plain text stdout/stderr bytes
```

SLIP escaping:

```text
0xa5 -> 0xdb 0xde
0xc0 -> 0xdb 0xdc
0xdb -> 0xdb 0xdd
0x0d -> 0xdb 0xd0
0x0a -> 0xdb 0xd1
```

The CR/LF escapes are required because firmware frames are written through
ESP-IDF console VFS outputs that can translate `\n` to `\r\n`. The host must
reverse these escapes before CRC verification; the CRC is over the original
unescaped bytes.

CRC is CRC-16-CCITT (CCITT-FALSE): polynomial `0x1021`, initial value `0xffff`,
no reflection, no final xor. It is computed over the unescaped frame type byte
and payload bytes only; the two resulting bytes are appended little-endian and
then SLIP-escaped before the closing `0xc0`. The firmware uses a 256-entry LUT.

The transport payload cap is 3072 bytes. Text writes can arrive as multiple text
frames. On9log binary packets that exceed the cap are dropped by the UART/VFS
transport; the next received on9log packet should show a sequence gap.

A normal LOG packet payload is:

```text
uint8_t arg_count
uint8_t arg_types[arg_count]
encoded arguments in printf format order
```

Argument type values (`ON9_LOG_ARGS_TYPE_*`):

```text
0 NONE           1 32BITS    2 64BITS    3 POINTER    4 DYNAMIC_STRING
5 DYNAMIC_STRING_VIEW
```

Encoded argument values:

```text
32-bit argument       4 bytes
64-bit argument       8 bytes
pointer argument      4 bytes (32-bit address)
dynamic string        uint32_t length + bytes[length], no trailing NUL
null dynamic string   uint32_t 0xffffffff, no following bytes
string view           uint32_t length + bytes[length], no trailing NUL
```

For `%.*s`, the precision argument is encoded as a normal 32-bit argument before
the string argument, and the host applies precision when rendering. By default,
firmware does not scan original format literals because that can retain them in
the flashed binary. If `ON9LOG_ENABLE_FORMAT_SCAN_HINT=1`, C macro callsites
pass the original format literal as a scan hint only when a string argument is
present, so firmware can copy the marked string using the preceding
non-negative precision byte count instead of relying on NUL termination. The
emitted `fmt_id` still resolves to the `.noload` format string.

A BUFFER packet payload is:

```text
uint32_t total_len
uint32_t offset
uint32_t chunk_len
uint8_t  bytes[chunk_len]
```

A DROPPED packet payload is `uint32_t dropped_count` (device-side logger drops,
distinct from transport loss detected by sequence gaps).

## Crate Layout

```text
host_cli/
  Cargo.toml                 workspace root and shared dependency versions
  Cargo.lock
  on9log-protocol/
    Cargo.toml               protocol library; deps: crc, goblin, addr2line, sprintf
    src/
      lib.rs                 protocol crate root, public re-exports
      wire.rs                header/types/levels/arg-type constants and Header::parse
      crc.rs                 CRC-16-CCITT-FALSE (table-generated to match firmware LUT)
      framer.rs              typed transport SLIP deframer + CRC verification + raw text
      elf_resolv.rs          goblin/addr2line address -> string/symbol/location resolver
      printf.rs              printf rendering via the `sprintf` crate (+ minimal scan)
      cppfmt.rs              C++23 `std::format`-style rendering via `std::fmt` (+ spec pre-processor)
      decode.rs              stateful Decoder: arg decode + packet dispatch + seq gaps
      crash.rs               ESP panic reason/backtrace recognizer and annotator
  on9log-cli/
    Cargo.toml               CLI binary; deps: clap, tokio-serial, tokio,
                              crossterm, libc, on9log_protocol
    src/
      term.rs                terminal width via `crossterm` + ANSI colors + word wrap
      main.rs                CLI binary (clap + tokio-serial)
  on9log-capture/
    Cargo.toml               capture/replay binary; deps: clap, tokio-serial,
                              tokio, crossterm, libc, rusqlite, on9log_protocol
    src/
      term.rs                colors, width, word wrap, host-time formatting
      db.rs                  SQLite capture store/replay (raw outcomes + timestamps)
      main.rs                `capture` + `decode` subcommands (clap)
```

`on9log-capture` is the customer/developer split tool. `capture` records the
deframed transport stream verbatim to SQLite (on9log binary frames, SLIP-framed
plain text, raw plain text, and deframer diagnostics), each stamped with the
host wall-clock millisecond at receive time; it needs no ELF, because the
customer does not have the secret format strings. `decode` replays a captured
database through the same `Decoder`/`CrashDecoder` pipeline against an optional
`--elf`, producing human-readable text. Storage is ELF-independent: only the
developer who holds the ELF can turn the captured addresses into strings.

### Library public surface

```rust
use on9log_protocol::{Deframer, Decoder, DecodedPacket, ElfStrings, Outcome};

let mut deframer = Deframer::new();
let mut decoder = Decoder::new();
let elf = ElfStrings::from_bytes(&elf_bytes).ok();

for outcome in deframer.feed(&uart_bytes) {
    if let Outcome::Frame(frame) = outcome {
        let pkt = decoder.decode(&frame, elf.as_ref());
        // match on DecodedPacket::Log / Buffer / Dropped / Other / Malformed
    }
}
```

The decoding pipeline is intentionally synchronous and allocation-light so a
`napi-rs` binding can expose `Deframer`, `Decoder`, and `ElfStrings` directly
and feed chunks from any transport.

### Module responsibilities

The following modules live in `on9log-protocol/src`.

`wire.rs` mirrors `on9log_fmt.h` and the `ON9_LOG_ARGS_TYPE_*` values from
`on9log.h`: `PacketType`, `Level`, `ArgType`, and `Header::parse` (returns
`None` on magic mismatch or unknown type/level nibbles).

`crc.rs` computes CRC-16-CCITT-FALSE the same way the firmware's LUT does
(`crc = (crc << 8) ^ table[((crc >> 8) ^ byte) & 0xff]`). The table is generated
at runtime from polynomial `0x1021`; the standard check value "123456789" ->
`0x29b1` is covered by a unit test.

`framer.rs` holds the streaming `Deframer`. Feed it arbitrary byte slices.
Seeing `0xa5` starts a transport frame. Bytes are then SLIP-unescaped until
`0xc0`, which ends the frame and triggers CRC/type validation. Outside explicit
transport frames, every byte except the explicit `0xa5` frame-start marker is
surfaced as `Outcome::PlainText`, so arbitrary raw console text is preserved
byte-for-byte. An unescaped `0xa5` inside a frame is treated as a resync point:
the partial frame is discarded and a fresh frame begins.
`decode_frame` returns one of:

- `Outcome::Frame(RawFrame)` — header parsed, CRC verified;
- `Outcome::PlainText(Vec<u8>)` — verified transport frame type `0x02` or
  printable raw UART text outside transport frames;
- `Outcome::BadMagic` — magic byte not `0x9a`;
- `Outcome::CrcMismatch` — CRC did not verify;
- `Outcome::Truncated` — fewer than header + CRC bytes;
- `Outcome::LengthMismatch` — non-streaming payload length did not match the
  header's declared `payload_len`;
- `Outcome::FrameTooLong` — decoded transport frame exceeded 3072 bytes;
- `Outcome::UnknownFrameType(_)` — verified frame type was not `0x01` or `0x02`;
- `Outcome::InvalidEscape` — bad SLIP escape sequence.

Non-Frame outcomes never halt decoding. Bad SLIP frames are reported and the
deframer returns to start-marker hunt mode. For non-streaming on9log packets
(`payload_len != 0xffff`) the deframer also checks that the payload length
matches the declared value.

`elf_resolv.rs` parses the firmware ELF with goblin and builds an
address-indexed table of string-bearing sections and function symbols. When
loaded from a path (`ElfStrings::from_path`), it also creates an `addr2line`
loader for DWARF file/line resolution. It skips NOBITS/SHT_NOBITS sections,
which carry no file bytes. It normally skips VMA-0 sections too, but keeps VMA-0
sections whose name contains `.noload`, because ESP-IDF's no-load format-string
output section is kept in the ELF with file bytes at address 0.

Format and tag resolution are intentionally separate:

- `read_format(addr)` reads from section names containing `.noload` first, then
  falls back to any string-bearing ELF section for plain `on9log.hpp`
  `logger.info("...", ...)` formats;
- `read_tag(addr)` reads from non-`.noload` sections.

If multiple matching sections cover the same address and produce different
strings, lookup fails instead of silently choosing one. When no ELF is supplied,
or lookup fails, addresses render as `@0x........` / `<fmt @0x........>`.

`printf.rs` renders a format string against decoded `Arg` values using the
`sprintf` crate (`sprintf::vsprintf`), which performs all digit conversion,
padding, hex/float formatting, etc. Two things require a small amount of glue
around `vsprintf`:

- `sprintf` dispatches by the *Rust type* of each argument (via downcast) and is
  strict: `%d` wants a signed int, `%u`/`%x` an unsigned int, `%c` a `u32`, `%f`
  an `f64`, `%p` a raw pointer, `%s` a string. The on9log wire only carries raw
  32/64-bit values without signedness, so the wire alone cannot pick the right
  Rust type. `printf.rs` scans the format string to recover each conversion
  character and coerces each wire argument to the matching Rust type. `sprintf`'s
  own parser collapses `d`/`i`/`u` into one `ConversionType` variant, so it
  cannot make this call itself.
- `sprintf` 0.4.3 mishandles dynamic precision (`%.*s`): the `*` argument is
  rejected for every integer type. `printf.rs` works around this by resolving
  `*` width/precision arguments to literal values while scanning, so `vsprintf`
  only ever receives value arguments. Negative width means left-justify (C
  semantics); negative precision is omitted.

The scanner does no formatting itself. `%.*s` consumes the precision argument
before the string argument, matching the firmware's argument ordering. Null
dynamic strings render as `(null)`.

`printf.rs` also exposes `render_format(fmt, args)`, the dispatcher used by
`decode.rs`. The dispatch rule is **C++ only if the string contains a C++23
replacement field *and* no active printf conversion** (`has_printf_conversion`
scans for a non-`%%` `%` ending in a conversion char). Mixed strings such as
`"payload={} status=%d"` stay on the printf path so `%d` consumes its argument
and `{}` is treated as a literal — this matches the firmware macro's
`format(printf, 3, 5)` annotation, which is how the compiler interprets the
string. Pure `{}`-style strings (no `%` conversions) route to `cppfmt::render`,
so a single firmware ELF can carry a mix of `%`-style and `{}`-style format
strings and each is decoded by the appropriate renderer. Note: a bare `%`
followed by a conversion-looking sequence (e.g. `% d`, which is a valid printf
space-flag conversion) wins the printf path; C++-style authors must not leave an
unescaped `%` immediately before a conversion letter.

`cppfmt.rs` renders C++23 `std::format`-style format strings (`{}`, `{:d}`,
`{0:>10.2f}`, ...) against the same decoded `Arg` values, using Rust's
`std::fmt` for the core value conversion. `std::fmt` cannot parse a *runtime*
format string (`format!`/`format_args!` require a literal and
`fmt::Arguments` has no public dynamic constructor), so the module is split
into a spec pre-processor and a value renderer:

- The pre-processor parses the C++23 field grammar
  (`[fill align][sign][#][0][width][.precision][type]`) into a `Spec` and
  interprets the type character. Rust's format spec has no `d` (decimal-forced),
  `s` (string-forced), or `c` (char-from-codepoint) type characters, so those
  are handled here by picking the right Rust trait/conversion before padding.
  `x`/`X`/`o`/`b`/`e`/`E` map to `LowerHex`/`UpperHex`/`Octal`/`Binary`/
  `LowerExp`/`UpperExp`; `p` renders `0x` + lowercase hex; `f`/`F` use `Display`
  with precision (fixed); `g`/`G` use `{:?}` shortest round-trip. Width and
  precision may themselves be nested replacement fields (`{:{}}`, `{:.{}}`,
  `{0:{1}}`); a referenced argument must be an integer, and a negative dynamic
  width/precision is a render error (C++23 semantics, also guards against a
  stray `0xffffffff` producing a multi-gigabyte pad). Auto arg-id assignment
  order within one field is value -> width -> precision.
- The renderer produces the core string via `format!` with literal specs
  (`format!("{:x}", v)`, `format!("{:.prec$e}", xa, prec = p)`, ...), then
  applies sign, `#` prefix, `0` zero-pad-inside, integer precision (minimum
  digits, zero-padded after the prefix; precision 0 with value 0 yields the
  empty string), and width/fill/align itself. The `0` flag is ignored when an
  align is present (any type) or when a precision is specified for integer
  types.

`{}` defaults follow the wire argument type: `Arg::Str` -> string, `Arg::Ptr`
-> `0x` address, `Arg::U32`/`Arg::U64` -> unsigned decimal, `Arg::U64` with a
float type -> `f64` (bit-cast). Automatic (`{}`) and explicit (`{0}`) arg ids
cannot be mixed anywhere in the string, including in nested width/precision
refs (C++ format error). Supported type characters are `b B c d e E f F g G o p
s x X` plus empty. The intentionally unsupported C++ format features are:
`L` locale formatting (`{:L}` and friends), `a`/`A` hex-float formatting,
chrono format specs, tuple formatters, and range formatters. Using any of these
should yield a `<render error: ...>` marker so the log line stays visible rather
than being silently misdecoded. Detection (`looks_like_cpp`) treats a token as a
field only if `{` is followed by optional digits and then `:` or `}`, so printf
strings with literal braces like `json {key=%d}` are not misrouted.

Known limitations: `g` precision is not exactly C++ significant-digit
semantics; Rust's `LowerExp`/`UpperExp` use a 1-digit exponent (`e0`) versus
C++'s `e+00`; `#` for floats (forced decimal point) is ignored. The wire
carries no signedness and no int/float distinction for 64-bit values, so `{}`
on an `Arg::U64` renders the raw bit pattern as an unsigned integer — use
explicit type chars (`{:f}`, `{:d}`) when the default is ambiguous. Correcting
the `{}` default for signed/float 64-bit values is not a pure host fix: it
requires richer firmware wire metadata (e.g. separate signed/float type codes
in `ON9_LOG_ARGS_TYPE_*`), since `Arg::U64` today cannot tell `int64_t` /
`uint64_t` / `double` apart.

C++ formatting is host-side only: it does not change firmware logging cost or
wire size. The current implementation reparses each `{}`-style format string on
every decoded packet, which is acceptable for normal UART monitor rates but is
the obvious performance cost for high-rate replay or very chatty logs. If this
becomes measurable, the preferred fix is to cache parsed format plans by
`fmt_id` in `Decoder`: first resolve the ELF string, decide printf vs C++ once,
parse C++ specs once, then render future packets from the cached plan. Cache
parse failures too so a bad format string is not reparsed forever. Avoid deeper
micro-optimizations until a benchmark shows parsed-plan caching is insufficient.

`decode.rs` owns the stateful `Decoder`. It tracks `last_seq` and reports
sequence gaps using wrapping arithmetic (`gap = seq - (last + 1)`, correct across
`u16` wrap). It dispatches each packet type:

- LOG -> parse arg type table, decode args, resolve tag/fmt, render message;
- BUFFER -> parse total_len/offset/chunk_len, carry the raw bytes;
- DROPPED -> parse the dropped count;
- TIME_SYNC / BOOT -> surfaced as `DecodedPacket::Other` with the raw payload
  (the firmware does not currently emit these, but they are reserved for forward
  compatibility);
- any decode failure -> `DecodedPacket::Malformed { meta, reason }`.

`Decoder::reset()` clears sequence tracking (e.g. after a device reboot).

The following modules live in `on9log-cli/src`.

`term.rs` handles presentation. Terminal width is detected with
`crossterm::terminal::size()`, which performs the platform-specific
`ioctl(TIOCGWINSZ)` on Linux and macOS for us; it falls back to the `COLUMNS`
env var, then a default of 80. Colors are plain ANSI SGR codes. `wrap()`
word-wraps to a column count (hard-breaking tokens longer than the width), and
`print_log_line()` prints a colored prefix + first line wrapped to the remaining
width, then indents continuation lines under the message. Color emission is
suppressed when stdout is not a TTY (or when `--no-color` is passed).

`crash.rs` is a streaming recognizer for ESP panic text carried in plain-text
frames or raw UART text. It watches complete text lines for panic/crash markers
such as `abort() was called`, `Guru Meditation Error:`, `assert failed:`, and
`Backtrace:`. Backtrace entries are parsed as `PC:SP` pairs; only the PC half is
symbolicated. With an ELF loaded from a path, the CLI uses DWARF line info via
`addr2line` and function symbols via `goblin`, so annotations can include
`function at file:line`. Without matching debug info, addresses fall back to
function+offset or `<unresolved>`.

`main.rs` is the CLI. It uses clap with derive: `-p/--port` (required), `-b/--baud`
(default 115200), `--elf` (optional firmware ELF path), `--no-color`,
`-t/--timestamp`, `--width` (0 = auto-detect), `--no-esp-reset`, `-s/--save`,
and `--log-path`. It loads the ELF up front, builds a tokio runtime, opens the serial port via
`tokio_serial::new(port, baud).open_native_async()`, and by default performs an
ESP hard reset by releasing DTR/GPIO0, asserting RTS/EN for 100 ms, then
releasing RTS/EN and waiting another 100 ms. `--no-esp-reset` disables this
startup reset. The read loop uses 4096-byte chunks and feeds each chunk to the
deframer. When stdin is a TTY, the CLI also puts stdin into non-canonical,
no-echo input mode and exits on the two-key monitor sequence `Ctrl+]` followed
by `Ctrl+T`. On Unix this preserves output processing, so normal `\n` log output
continues to render correctly.

Verified plain-text transport frames and printable raw UART text are written to
stdout byte-for-byte unless `--timestamp` is set, in which case a local
wall-clock prefix is inserted at each text line start. The host does **not**
infer colors from `I/W/E/D/V` prefixes; ANSI SGR color bytes emitted by the
device are preserved. Verified on9log frames are decoded and printed with the
normal colored/wrapped presentation. Crash annotations are appended after the
original plain-text crash lines.

When `-s/--save` is enabled, the CLI writes a decoded human-readable text log to
disk. `--log-path <FILE>` chooses the output path; otherwise the default is
`on9log-UNIX_TIMESTAMP.log`, where the timestamp is seconds since the UTC Unix
epoch. Binary on9log packets are saved after host decoding, not as raw binary
frames. Plain-text device output is saved with ANSI escape sequences stripped so
the file stays readable. If `-t/--timestamp` is enabled, the same local timestamp
prefix is included in the saved file. File writes are flushed and `sync_data()`ed
after each decoded/logged item so the file is updated immediately.

## on9log-capture: capture to SQLite and replay against an ELF

`on9log-capture` is a second host binary (crate `on9log-capture`, binary
`on9log-capture`) that splits the workflow the `on9log` CLI conflates:

- **`capture`** (run by the customer, who does **not** have the firmware ELF with
  the secret format strings): opens a UART, deframes the typed SLIP+CRC transport
  with `on9log_protocol::Deframer`, and stores every raw outcome to a SQLite
  database. No ELF is loaded; nothing is decoded against an ELF at capture time.
  Each row is stamped with `captured_at_ms`, the host wall-clock millisecond at
  which that outcome was received (per-outcome, recorded before the store
  transaction commits, so the timestamp is receive time, not commit time).
  `capture` does not accept `--elf` and never prints captured log contents,
  including decoded messages, plain text, or argument dumps. It exits on
  `Ctrl+C` and stores plain-text bytes byte-for-byte, including ANSI color
  sequences.
- **`decode`** (run by the developer, who holds the matching `--elf`): opens a
  captured database and replays its rows in capture order through the same
  `Decoder` / `CrashDecoder` pipeline as the live `on9log` CLI. Without
  `--save`, it prints human-readable text to stdout. With `--save`, rendered
  text is written only to the requested file. `--no-color` disables generated
  colors and strips ANSI escape sequences from captured plain text before
  writing the selected output sink.

Storage is deliberately **ELF-independent**. The capture database holds the raw
18-byte on9log header and payload BLOBs (for frames), the raw text bytes (for
plain text), and a short code (for deframer diagnostics such as `crc_mismatch` or
`unknown_frame_type`). The parsed header fields are also written as normal
columns so a capture file can be queried directly (`WHERE level = 1`,
`ORDER BY seq`) without re-parsing. Reconstructing an `Outcome` from a row
re-parses the stored header bytes, so the decoded output is byte-identical to
what the live `on9log` CLI would have produced from the same stream.

Schema (`db.rs`):

```text
meta(key TEXT PRIMARY KEY, value TEXT)
events(
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  captured_at_ms  INTEGER NOT NULL,        -- host receive time, UTC ms
  kind            TEXT NOT NULL,           -- on9log | text | <error code>
  seq, device_time_ms, level, ptype, tag_id, fmt_id, payload_len  -- denormalized header
  header          BLOB,                    -- 18-byte on9log header (frames only)
  payload         BLOB,                    -- frame payload | text bytes
  detail          TEXT                     -- e.g. frame-type byte for unknown_frame_type
)
```

`capture` opens the database in WAL mode with `synchronous=NORMAL` and writes
each UART read's outcomes in one transaction. Appending to an existing capture
file is supported (AUTOINCREMENT continues). `decode` opens with SQLite
read-only flags plus the `query_only` pragma and errors if the path is missing
or lacks the `events` table, so it can never mutate a customer's capture.

`decode` uses one stateful `Renderer` (in `main.rs`) that wraps `Decoder` +
`CrashDecoder` and renders an `Outcome` exactly as the live CLI does. The
difference from the live tool is the timestamp source: the prefix is always
derived from the stored per-event `captured_at_ms`. `decode` defaults to width 0
(no wrapping) so its output stays grep- and file-friendly; `--width` restores
wrapping. Crash backtrace addresses are re-symbolicated on decode against the
supplied ELF, so a customer captures raw panic text and the developer resolves
the symbols later.

`term.rs` mirrors `on9log-cli`'s `term.rs` (colors, width, word wrap) and adds
`host_now_ms()` (portable `SystemTime`) and `local_ts_string(unix_ms)`, which
uses `localtime_r` on Unix and falls back to a UTC breakdown otherwise.

## ELF No-Load String Resolution

Format and tag strings take **different ELF addresses**, which is why the
resolver splits lookup by section family rather than doing one generic scan:

- **`fmt_id`** — `ON9_LOG_NOLOAD_STR(format)` wraps constant format strings in
  `ON9_LOG_NOLOAD_ATTR`, which emits input sections `.noload_keep_in_elf.<n>`.
  ESP-IDF's linker captures these into a dedicated ELF-only output section, so
  `fmt_id` is a **small offset near 0**, not a loadable rodata address. The
  `on9log.hpp` C++ wrapper can also produce no-load formats via `ON9FMT("...")`,
  `logger.info<"...">(...)`, or `"..."_on9fmt`. Its plain
  `logger.info("...", ...)` overload uses ordinary function arguments, so those
  literals remain in normal read-only sections; `ElfStrings::read_format()`
  checks no-load sections first, then falls back to ordinary string-bearing ELF
  sections for that wrapper.
- **`tag_id`** — tags are passed straight through (`ON9_LOG_LEVEL(tag, ...)`)
  and land in ordinary `.rodata`, so `tag_id` is a real loadable address
  (e.g. `0x3f4xxxxx` on ESP32-S3).

The ESP-IDF linker rule that produces this lives in
`components/esp_system/ld/ld.debug.sections`:

```text
.noload 0 (INFO) :
{
    . = 0;
    LONG(0);                                   /* 4-byte NULL reservation */
    _noload_keep_in_elf_start = ABSOLUTE(.);
    SECTION_MAPPINGS(noload_keep_in_elf)
    KEEP(*(.noload_keep_in_elf .noload_keep_in_elf.*))
    _noload_keep_in_elf_end = ABSOLUTE(.);
}
```

Consequences the resolver depends on:

- `0` → output section VMA is 0; the first real format string starts at offset 4
  (after `LONG(0)`).
- `(INFO)` → non-allocatable (`SHF_ALLOC` unset), so it is excluded from the
  flashed binary image and from `PT_LOAD` segments, but **retained in the ELF**.
- `KEEP(...)` → file bytes are not garbage-collected; the section is **PROGBITS**
  with a real file offset, never NOBITS (initialized `const char[]` data is
  PROGBITS by nature; NOBITS is reserved for the `(NOLOAD)` BSS-style sections
  fed by `.bss`-class input).

This is why `elf_resolv.rs` keeps VMA-0 sections whose name contains `.noload`
(and skips all other VMA-0 sections such as `.debug_*` / `.comment`, which the
firmware never points into). `is_noload_section` uses `name.contains(".noload")`;
ESP-IDF also has a `.flash.rodata_noload (NOLOAD)` BSS section, but that is
NOBITS and excluded by the `sh_type == 8` check, and the firmware never emits a
`fmt`/`tag` address into it.

If format strings ever stop resolving (lines render as `<fmt @0x...>`), confirm
the built ELF still matches these assumptions:

```bash
readelf -S <firmware.elf> | grep noload   # expect: .noload at 00000000, PROGBITS, non-zero size
readelf -x .noload <firmware.elf> | head  # expect: format strings as ASCII starting at offset 4
```

If the section name, type, or addressing changes (e.g. a future linker-script
rewrite makes it NOBITS or moves it to a real VMA), the resolver's
`.noload`-name and VMA-0 special-casing must be revisited.

## Known Implementation Risks

The host tool currently passes its Rust unit tests and `cargo clippy -- -D
warnings`, but these review findings are still open:

- **LOG payloads are not fully strict.** `decode_log()` consumes the declared
  argument count and argument bodies, but it does not currently reject extra bytes
  left at the end of the payload. BUFFER and DROPPED packets already check exact
  lengths.
- **Formatter correctness depends on firmware ABI assumptions.** `printf.rs`
  assumes the ESP32 32-bit ABI for default `int`, `long`, `size_t`, pointers, and
  `ptrdiff_t`, and delegates conversion behavior to the `sprintf` crate. It is
  suitable for the current ESP32-S3 target, but it is not a general cross-target
  printf implementation.
- **Unsupported or unusual printf conversions render as errors.** On a
  `sprintf` error the CLI preserves the log by printing `<render error: ...>` plus
  the original format string. This is intentional recovery, not complete printf
  coverage.
- **C++23 `{}` defaults cannot recover signedness/float-ness from the wire.**
  `cppfmt.rs` `{}`
  renders `Arg::U64` as an unsigned integer because the on9log wire
  (`ON9_LOG_ARGS_TYPE_*`) only distinguishes 32-bit / 64-bit / pointer / string,
  not signed vs. unsigned vs. `double`. `{:d}`, `{:f}`, etc. work correctly via
  explicit type chars; the ambiguous `{}` default is a firmware wire-format
  limitation, not a pure host bug, and needs richer argument type metadata to
  fix.
- **C++23 rendering reparses format strings today.** This is a host-side CPU
  cost only. The first performance fix should be a `fmt_id` keyed cache of
  parsed render plans in `Decoder`, including cached parse failures. That avoids
  repeating C++ field parsing and printf/C++ dispatch for hot log sites.
- **Decoded on9log messages lose repeated whitespace when wrapped.** `term::wrap`
  splits on whitespace, so multiple spaces/tabs inside decoded messages are
  collapsed. Verified type `0x02` text transport frames and raw UART text are
  written raw and are not affected.
- **Raw UART passthrough treats `0xa5` as transport start.** Outside explicit
  transport frames, every other byte is forwarded as raw text. A literal `0xa5`
  byte in unframed output starts a candidate transport frame; if arbitrary
  binary output can contain that byte, wrap it in a type `0x02` text frame.

## Output Format

A decoded LOG line looks like:

```text
I (   1234) TAG: rendered message goes here
```

where `I` is the ESP-IDF level letter, the parenthesized number is `time_ms`
since boot, then the tag, then the rendered message. Continuation lines (when
the message wraps) are indented under the message.

With `-t/--timestamp`, decoded log lines and plain-text transport lines are
prefixed with local wall time:

```text
[YYYYmmdd-hh:mm:ss.sss] I (   1234) TAG: rendered message goes here
```

Level colors:

```text
ERROR   red
WARN    yellow
INFO    green
others  white  (DEBUG, VERBOSE, NONE)
```

Buffer-dump packets print a header line followed by a hex+ASCII dump.
DROPPED packets and sequence gaps print dim yellow warning lines. Malformed
frames and undecodable payloads print diagnostics to stderr and do not stop
decoding.

## Decisions And Tradeoffs

- **18-byte header.** AGENTS.md describes the header as "fixed-header" without
  restating the size; the packed `on9log_packet_header_t` is
  1+1+2+4+4+4+2 = 18 bytes, so `HEADER_LEN = 18`.
- **Workspace split.** `on9log_protocol` has no `tokio`, serial-port, terminal,
  or clap dependency; only `on9log_cli` needs those runtime/presentation crates.
  This keeps a future `napi-rs` binding small and sync-friendly.
- **goblin for ELF.** Resolves `fmt_id`/`tag_id` addresses to C strings. Format
  strings are resolved from section names containing `.noload` first, including
  ESP-IDF's VMA-0 ELF-only no-load output section, then from ordinary
  string-bearing sections for plain `on9log.hpp` formats. Tags are resolved
  from normal non-`.noload` sections. goblin is the user-chosen parser.
- **sprintf for printf rendering.** `%`-style format strings are rendered by
  the `sprintf` crate, not a hand-rolled renderer. Because `sprintf` dispatches
  by Rust type and the wire carries no signedness, `printf.rs` scans the format
  to coerce each argument to the correct type (`i8`/`u8` for `hh`, `i16`/`u16`
  for `h`, `i32`/`u32` for default/32-bit ESP32 `long`, `i64`/`u64` for `ll`,
  `f64` for floats, raw pointer for `%p`, `String` for `%s`). It also resolves
  `*` width/precision to literals to work around a `sprintf` 0.4.3 bug where
  `%.*s` rejects its `*` argument. The scanner does no formatting.
- **std::fmt for C++23 rendering, with a spec pre-processor.** `{}`-style
  format strings are rendered by `cppfmt.rs`, which uses Rust's `std::fmt`
  traits for core value conversion and a hand-written pre-processor for the
  C++23 spec grammar (because `std::fmt` cannot parse a runtime format string).
  `printf::render_format` dispatches per string: C++ only if there is a C++
  field and no active printf conversion, so mixed strings stay on the printf
  path. `d`/`s`/`c` type chars (which Rust's format spec lacks) and nested
  dynamic width/precision (`{:{}}`) are handled by the pre-processor.
- **crossterm for terminal size.** Window width comes from
  `crossterm::terminal::size()` (cross-platform `ioctl`); colors remain raw ANSI
  SGR codes emitted inline so wrapped lines stay colored.
- **Streaming deframer.** The deframer accepts arbitrary byte boundaries,
  forwards printable raw text outside frames, and accumulates from a starting
  `0xa5` until a frame-ending `0xc0`, so partial reads and split frames are
  handled naturally. If the opening `0xa5` is missed, printable fragments from
  the damaged data may be shown, but the parser still hunts until the next
  unescaped `0xa5` and resynchronizes.
- **ESP reset on monitor startup.** The CLI resets by default because it is
  primarily a monitor-like UART tool. The sequence mirrors the common ESP
  active-low wiring: DTR false (GPIO0 released), RTS true (EN asserted low),
  100 ms delay, RTS false (EN released), 100 ms delay. Use `--no-esp-reset` for
  RAM-loaded apps or when attaching without rebooting.
- **Monitor quit sequence.** Interactive CLI sessions quit with `Ctrl+]` then
  `Ctrl+T`, matching monitor-style tools that avoid using `Ctrl-C` as the normal
  escape path. The sequence is detected from stdin while serial reads continue
  asynchronously.
- **Save file output.** `-s/--save` writes decoded text, not transport bytes.
  ANSI is stripped only for the file sink; stdout still preserves device ANSI
  bytes. Immediate persistence uses `flush()` plus `sync_data()` after writes,
  favoring crash-time durability over maximum throughput.
- **Crash decoding is opportunistic.** Panic text is still printed exactly as
  received. The host adds best-effort annotation lines when it sees known ESP
  panic/backtrace patterns and has enough ELF/DWARF information to resolve PCs.
- **Lenient recovery.** Bad magic, CRC mismatch, invalid escapes, oversized
  frames, and truncation are reported but do not halt the stream; the next
  unescaped `0xa5` resynchronizes.
- **printf scope.** Rendering is delegated to `sprintf`, which covers the
  conversions used by ESP-IDF-style log formats. On a `sprintf` error the format
  string is returned with a marker so the log line is not lost.
- **C++23 renderer scope.** `cppfmt.rs` implements the common `std::format`
  cases used in firmware logs. It intentionally does not support `{:L}` locale
  formatting, `{:a}`/`{:A}` hex-float formatting, C++ chrono format specs, tuple
  formatters, or range formatters; those should surface as `<render error: ...>`
  markers. `{}` cannot recover signed/float 64-bit semantics from the wire (see
  Known Implementation Risks); explicit type chars work.
- **Cross-platform.** Linux and macOS are supported; `crossterm` handles the
  platform-specific terminal-size syscall. Other platforms fall back to the
  `COLUMNS` env var / default width.

## Building And Running

```bash
cd host_cli
cargo build --release

# basic usage
./target/release/on9log -p /dev/ttyUSB0 -b 115200

# with ELF string resolution
./target/release/on9log -p /dev/ttyUSB0 -b 115200 --elf ../build/myapp.elf
```

CLI flags:

```text
-p, --port <PORT>     UART device path (e.g. /dev/ttyUSB0)        required
-b, --baud <BAUD>     baud rate                                    default 115200
    --elf <FILE>      firmware ELF for format/tag string resolution
    --no-color        disable colored output
-t, --timestamp       prefix logs/text lines with local wall time
    --width <WIDTH>   override terminal width (0 = auto-detect)    default 0
    --no-esp-reset    do not toggle DTR/RTS to reset on startup
-s, --save            save decoded human-readable text log to file
    --log-path <FILE> path for --save output; default on9log-UNIX_TIMESTAMP.log
```

Tests:

```bash
cargo test --workspace
                 # CRC, sprintf rendering, framing/raw text, crash decode,
                 # ELF resolution, terminal wrapping, and CLI helpers
cargo clippy     # 0 warnings
```

## Future Work

- `napi-rs` binding exposing `on9log_protocol::{Deframer, Decoder, ElfStrings}`
  and the decoded record types to Node.js.
- TIME_SYNC / BOOT packet decoding once the firmware emits them (currently
  surfaced as opaque `Other` payloads). Time sync would let the host map
  boot-relative `time_ms` to UTC.
- Optional sequence-gap statistics / loss-rate reporting.
- Optional JSON output mode for piping into other tooling.
- TCP / WebSocket transport sources alongside UART (the decoder is transport-
  agnostic; only `on9log-cli/src/main.rs` is UART-specific).
