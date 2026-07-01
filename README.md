# on9log — binary logger host tool

**on9log** is the host-side decoder and monitor for a binary log stream emitted by
ESP32 firmware. The firmware logger strips printf/C++ format strings to a
`.noload` ELF section at compile time and transmits only fixed-size binary
packets over UART, avoiding firmware binary bloat and speeding up compilation.
The host tool reconstructs human-readable log output by decoding packets against
the firmware ELF.

It works similarly to Rust's `defmt` or Espressif's `esp-log` binary mode: the
firmware carries no format strings, the ELF carries all the secrets.

## What's inside

This workspace contains three crates:

| Crate | Type | Description |
|-------|------|-------------|
| `on9log-protocol` | library | Decoding pipeline, SLIP+CRC deframer, printf & C++23 rendering, ELF/DWARF resolution, crash backtrace annotation. No async runtime dependency — reusable from `napi-rs` or other bindings. |
| `on9log-cli` | binary (`on9log`) | UART monitor and file/stdin stream decoder. Deframes the stream, resolves addresses against an ELF, and prints colorized wrapped output with save-to-file support. |
| `on9log-capture` | binary (`on9log-capture`) | Split customer/developer workflow. `capture` records the raw transport stream to SQLite (no ELF needed). `decode` replays a capture against an ELF to produce human-readable text. |

## Quick start

```bash
# Build everything
cargo build --release

# Live monitor with ELF string resolution
./target/release/on9log -p /dev/ttyUSB0 -b 115200 --elf firmware.elf

# Decode a stream captured from the Unix demo. Pass the matching host
# executable: ELF on Linux, Mach-O on macOS.
./target/release/on9log --log-bin ../on9log.bin --elf ../build-binary/on9log_unix_demo

# Decode the Unix demo directly through a pipe
../build-binary/on9log_unix_demo | ./target/release/on9log --log-stdin --elf ../build-binary/on9log_unix_demo

# Capture raw stream (customer — no ELF required)
./target/release/on9log-capture capture -p /dev/ttyUSB0 -b 115200 -o session.db

# Decode capture later (developer — with ELF)
./target/release/on9log-capture decode session.db --elf firmware.elf
```

## on9log CLI

```
on9log <--port PORT|--log-bin FILE|--log-stdin> [--elf FILE] [--no-color] [-t] [-s] [--log-path FILE]
```

| Flag | Description |
|------|-------------|
| `-p, --port` | Read a live UART device |
| `--log-bin` | Replay a binary stream captured from the logger's stdout |
| `--log-stdin` | Read a live binary stream from stdin, suitable for shell pipelines |
| `-b, --baud` | UART baud rate (default 115200; ignored for file/stdin input) |
| `--elf` | Matching ELF firmware/host executable, or macOS Mach-O host executable, for format/tag resolution |
| `--no-color` | Disable ANSI color output |
| `-t, --timestamp` | Prefix each line with local wall-clock time |
| `--width` | Override terminal width (0 = auto-detect) |
| `--no-esp-reset` | Skip DTR/RTS reset on UART startup |
| `-s, --save` | Save decoded text log to file |
| `--log-path` | Output path for --save (default: `on9log-<unix_ts>.log`) |

Exactly one input option is required. `--log-bin` exits at end-of-file;
`--log-stdin` exits when its producer closes the pipe. File and stdin input use
the same streaming deframer, decoder, rendering, timestamps, and save behavior
as UART input, but do not perform ESP reset or install interactive monitor keys.

On macOS, the binary demo writes one `@on9log-image-slide=...` metadata line
before its framed stream. Replay consumes this line internally and applies the
ASLR slide while resolving 32-bit format/tag IDs against the matching Mach-O
executable; the metadata line is not printed as device output. Linux builds the
binary demo as non-PIE so its IDs resolve directly against the matching ELF.

Output uses standard ESP-IDF log formatting:

```
I (   1234) TAG: rendered message goes here
[20260626-14:30:01.234] I (   1234) TAG: rendered message goes here   # with -t
```

Level colors: ERROR red, WARN yellow, INFO green, others white.

## on9log-capture — split workflow

The capture tool separates collection from decoding so customers can capture
logs without access to the firmware ELF (which contains secret format strings).

- **`capture`** records every deframed transport outcome (on9log binary frames,
  plain text, deframer diagnostics) to a SQLite database with host wall-clock
  timestamps. No ELF is loaded or needed at capture time.
- **`decode`** replays a captured database through the same decoder pipeline as
  the live CLI, resolving addresses against the supplied `--elf`. Output goes to
  stdout or a `--save` file.

The SQLite schema stores raw header and payload BLOBs plus denormalized header
fields (`level`, `seq`, `tag_id`, `fmt_id`, etc.), so captures are directly
queryable with standard SQL.

## Library usage

```rust
use on9log_protocol::{Deframer, Decoder, ElfStrings};

let mut deframer = Deframer::new();
let mut decoder = Decoder::new();
let elf = ElfStrings::from_bytes(&elf_bytes).ok();

for outcome in deframer.feed(&uart_chunk) {
    if let on9log_protocol::Outcome::Frame(frame) = outcome {
        let pkt = decoder.decode(&frame, elf.as_ref());
        // match on DecodedPacket::Log / Buffer / Dropped / Other / Malformed
    }
}
```

The protocol crate is synchronous and allocation-light — it carries no async
runtime, serial-port, or terminal dependencies. It's designed to be wrapped
by a future `napi-rs` binding for Node.js, or embedded in any other transport
frontend (TCP, WebSocket, file replay, etc.).

## Wire format

The firmware sends typed SLIP frames with CRC-16-CCITT verification over raw
UART. Each on9log binary packet has an 18-byte header:

```
magic (0x9a) | type+level | seq (u16) | time_ms (u32) | tag_id (u32) | fmt_id (u32) | payload_len (u16)
```

Format strings live in a `.noload` ELF output section at VMA 0 — kept in the
ELF file but excluded from the flashed binary image. The host resolves
`fmt_id` addresses against this section, and `tag_id` addresses against normal
`.rodata` sections.

Both C `printf`-style (`%d`, `%s`, `%.*s`) and C++23 `std::format`-style
(`{}`, `{0:>10.2f}`, `{:#x}`) format strings are supported, dispatched
automatically per string. The host also recognizes ESP panic/backtrace patterns
in plain-text output and annotates them with resolved function names and source
locations when DWARF debug info is available.

## Building

```bash
cargo build --release
cargo test --workspace
cargo clippy -- -D warnings
```

Requires Rust 1.85+ (edition 2024). Linux and macOS are supported; other
platforms fall back to `COLUMNS` env var for terminal width.

## License

MIT — see the `license` field in each crate's `Cargo.toml`.

## Future work

- `napi-rs` binding for the protocol crate
- TIME_SYNC / BOOT packet decoding
- Sequence-gap statistics and loss-rate reporting
- Optional JSON output mode
- TCP / WebSocket transport sources (the decoder is already transport-agnostic)
