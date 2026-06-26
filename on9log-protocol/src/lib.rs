//! Protocol decoder library for on9log binary log streams.
//!
//! This crate is usable both as a library (e.g. via a future `napi-rs` binding)
//! and as the backing for the `on9log` CLI binary. It contains the decoding,
//! framing, crash-text recognition, printf rendering, and ELF symbolication
//! logic, but no UART runtime or terminal presentation code.
//!
//! Typical library usage:
//!
//! ```
//! use on9log_protocol::{Deframer, Decoder, ElfStrings};
//!
//! let mut deframer = Deframer::new();
//! let mut decoder = Decoder::new();
//! let elf = ElfStrings::from_bytes(&[]).ok();
//! let uart_bytes: [u8; 0] = [];
//!
//! for outcome in deframer.feed(&uart_bytes) {
//!     if let on9log_protocol::Outcome::Frame(frame) = outcome {
//!         let _pkt = decoder.decode(&frame, elf.as_ref());
//!     }
//! }
//! ```

pub mod cppfmt;
pub mod crash;
pub mod crc;
pub mod decode;
pub mod elf_resolv;
pub mod framer;
pub mod printf;
pub mod wire;

/// Re-export [`CrashDecoder`] — a streaming panic-text annotator for ESP-IDF crashes.
pub use crash::CrashDecoder;
/// Re-export core decode API: [`Decoder`], [`DecodedPacket`], and record types.
pub use decode::{BufferRecord, DecodedPacket, Decoder, DroppedRecord, LogRecord, PacketMeta};
/// Re-export ELF resolution types for string and symbol lookups.
pub use elf_resolv::{ElfStrings, ResolvedSymbol, SourceLocation};
/// Re-export transport deframing types: [`Deframer`], [`Outcome`], [`RawFrame`].
pub use framer::{Deframer, Outcome, RawFrame};
/// Re-export printf-style argument type and top-level dispatch renderer.
pub use printf::{Arg, render_format};
/// Re-export wire protocol primitives: [`ArgType`], [`Header`], [`Level`], [`PacketType`].
pub use wire::{ArgType, Header, Level, PacketType};
