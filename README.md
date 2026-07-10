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
| `on9log-cli` | binary (`on9log`) + web UI | UART monitor and file/stdin stream decoder. Deframes the stream, resolves addresses against an ELF or Mach-O image, prints colorized wrapped output, and exposes an Axum REST/WebSocket backend plus a Bun/React web terminal under `on9log-cli/web`. |
| `on9log-capture` | binary (`on9log-capture`) | Split customer/developer workflow. `capture` records the raw transport stream to SQLite (no ELF needed). `decode` replays a capture against an ELF to produce human-readable text. |

## Quick start

```bash
# Build everything
cargo build --release

# Live monitor with ELF string resolution
./target/release/on9log -p /dev/ttyUSB0 -b 115200 --elf firmware.elf

# Live monitor plus Axum HTTP/WebSocket companion server
./target/release/on9log -p /dev/ttyUSB0 --elf firmware.elf --web

# Run the web UI during development
cd on9log-cli/web && bun run dev

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
on9log <--port PORT|--log-bin FILE|--log-stdin> [--elf FILE] [--no-color] [-t] [-s] [--log-path FILE] [--web [ADDR]]
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
| `--web [ADDR]` | Start the Axum REST/WebSocket server; defaults to `127.0.0.1:9090` and increments the port if busy, UART mode only |

Exactly one input option is required. `--log-bin` exits at end-of-file;
`--log-stdin` exits when its producer closes the pipe. File and stdin input use
the same streaming deframer, decoder, rendering, timestamps, and save behavior
as UART input, but do not perform ESP reset or install interactive monitor keys.

The existing UART implementation remains the default hardware workflow:
`--port` still carries its port and baud into the original tokio-serial event
loop, performs the same optional DTR/RTS reset, and retains the `Ctrl+] Ctrl+T`
quit sequence. Replay is implemented in a separate blocking reader path so
file/stdin support does not retrofit the UART loop. CLI tests cover the original
port, baud, and `--no-esp-reset` parsing; real serial behavior still requires a
connected device for verification.

With `--web`, the UART monitor also starts an Axum companion server. By default
it listens on `127.0.0.1:9090`; pass `--web 127.0.0.1:8787` to override the
listen address and starting port. If the selected port is already in use, the
server automatically tries the next port on the same address (`9091`, `9092`,
and so on) and prints the actual bound URL on startup.
When `--web` is active, the CLI also tries to open the actual bound URL in the
default browser. Browser-launch errors are ignored, so headless/local-service
environments still host the web server normally.

The `on9log` binary serves the bundled web UI from the same Axum server. During
`cargo build`, the `on9log-cli` build script embeds whatever files are currently
present in `on9log-cli/web/dist`; rebuild the frontend first when you want the
Rust binary to contain new UI assets. The Rust build fails if `web/dist` is
missing, empty, or missing `index.html`.

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/ws/logs` | WebSocket | Streams decoded text log records as text messages. Binary on9log packets are sent as rendered log lines; plain-text transport is forwarded as ANSI-stripped text chunks. |
| `/api/status` | GET | Returns JSON with `port`, `baud`, `uptime_ms`, and current websocket client count. |
| `/api/target/reset` | POST | Performs the same ESP hard reset sequence used at startup: DTR released, RTS asserted for 100 ms, then released. Decoder sequence tracking is reset after success. |
| `/api/serial/lines` | POST | Sets raw serial control lines. JSON body: `{"dtr":false,"rts":true}`; include either or both fields. |

Serial reset/control APIs are serialized through the monitor task rather than a
second serial handle, because `tokio-serial` does not support cloning the async
`SerialStream`.

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

cd on9log-cli/web
bun run build
bun run lint
bun run format:check

# To refresh the UI bundled in the Rust binary:
bun run build
cd ../..
cargo build --release
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
