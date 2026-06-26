//! on9log wire format: packet header, packet types, log levels, argument types.
//!
//! Mirrors `on9log_fmt.h` and the `ON9_LOG_ARGS_TYPE_*` values from `on9log.h`.
//! All multi-byte fields are little-endian.

/// Magic byte that begins every on9log packet header.
pub const PACKET_MAGIC: u8 = 0x9a;
/// Special payload-length value indicating a streaming (indeterminate-length) packet.
pub const PAYLOAD_LEN_STREAMING: u16 = 0xffff;
/// Size of a parsed on9log packet header in bytes.
pub const HEADER_LEN: usize = 18;
/// Sentinel length value representing a null dynamic string.
pub const NULL_STRING_LEN: u32 = 0xffff_ffff;
/// Maximum payload size allowed in a single transport frame (3 KiB).
pub const TRANSPORT_MAX_PAYLOAD: usize = 3 * 1024;
/// Transport frame type byte for on9log binary packets.
pub const TRANSPORT_FRAME_ON9LOG: u8 = 0x01;
/// Transport frame type byte for plain text (stdout/stderr) packets.
pub const TRANSPORT_FRAME_TEXT: u8 = 0x02;

/// CRC-16-CCITT (CCITT-FALSE) initial value, matching `esp_stdio_log_vfs.c`.
pub const CRC16_CCITT_INIT: u16 = 0xffff;

/// Transport framing bytes from `esp_stdio_log_vfs.c`.
/// SLIP frame-start marker byte.
pub const SLIP_START: u8 = 0xa5;
/// SLIP frame-end marker byte.
pub const SLIP_END: u8 = 0xc0;
/// SLIP escape byte — the byte that follows is the escaped form.
pub const SLIP_ESC: u8 = 0xdb;
/// Escaped form of [`SLIP_END`] (`0xc0`).
pub const SLIP_ESC_END: u8 = 0xdc;
/// Escaped form of [`SLIP_ESC`] (`0xdb`).
pub const SLIP_ESC_ESC: u8 = 0xdd;
/// Escaped form of [`SLIP_START`] (`0xa5`).
pub const SLIP_ESC_START: u8 = 0xde;
/// Escaped form of `\r` (`0x0d`).
pub const SLIP_ESC_CR: u8 = 0xd0;
/// Escaped form of `\n` (`0x0a`).
pub const SLIP_ESC_LF: u8 = 0xd1;

/// Packet type, stored in the high nibble of `type_level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    /// Normal log message with a format string and typed arguments.
    Log = 0,
    /// Notification that the device dropped one or more log packets.
    Dropped = 1,
    /// Time-synchronisation packet (reserved, not currently emitted).
    TimeSync = 2,
    /// Boot event packet (reserved, not currently emitted).
    Boot = 3,
    /// Buffer-dump packet carrying a chunk of a memory/binary buffer.
    Buffer = 4,
}

impl PacketType {
    /// Convert a raw byte to a [`PacketType`], returning `None` for unknown values.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Log),
            1 => Some(Self::Dropped),
            2 => Some(Self::TimeSync),
            3 => Some(Self::Boot),
            4 => Some(Self::Buffer),
            _ => None,
        }
    }
}

/// `on9log_level_t` values, stored in the low nibble of `type_level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Level {
    /// No level (used for non-log packets such as dropped notifications).
    None = 0,
    /// Error-level log message.
    Error = 1,
    /// Warning-level log message.
    Warn = 2,
    /// Info-level log message.
    Info = 3,
    /// Debug-level log message.
    Debug = 4,
    /// Verbose-level log message.
    Verbose = 5,
}

impl Level {
    /// Convert a raw byte to a [`Level`], returning `None` for unknown values.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::None),
            1 => Some(Self::Error),
            2 => Some(Self::Warn),
            3 => Some(Self::Info),
            4 => Some(Self::Debug),
            5 => Some(Self::Verbose),
            _ => None,
        }
    }

    /// Single-character ESP-IDF-style level tag (e.g. `I` for INFO).
    pub fn letter(self) -> char {
        match self {
            Self::None => 'N',
            Self::Error => 'E',
            Self::Warn => 'W',
            Self::Info => 'I',
            Self::Debug => 'D',
            Self::Verbose => 'V',
        }
    }
}

/// Argument type values from `ON9_LOG_ARGS_TYPE_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ArgType {
    /// No argument (placeholder, treated as zero).
    None = 0,
    /// 32-bit unsigned integer argument.
    Bits32 = 1,
    /// 64-bit unsigned integer argument (also used for `f64` bit patterns).
    Bits64 = 2,
    /// Pointer-width (32-bit on ESP32) argument.
    Pointer = 3,
    /// Dynamic string argument preceded by a 32-bit length.
    DynamicString = 4,
    /// Length-aware dynamic string argument preceded by a 32-bit length.
    DynamicStringView = 5,
}

impl ArgType {
    /// Convert a raw byte to an [`ArgType`], returning `None` for unknown values.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::None),
            1 => Some(Self::Bits32),
            2 => Some(Self::Bits64),
            3 => Some(Self::Pointer),
            4 => Some(Self::DynamicString),
            5 => Some(Self::DynamicStringView),
            _ => None,
        }
    }
}

/// Parsed 18-byte packet header.
#[derive(Debug, Clone, Copy)]
pub struct Header {
    /// Packet magic byte ([`PACKET_MAGIC`]).
    pub magic: u8,
    /// Packet type (log, dropped, buffer, etc.).
    pub ptype: PacketType,
    /// Log severity level.
    pub level: Level,
    /// Monotonic sequence number (wrapping, used for gap detection).
    pub seq: u16,
    /// Timestamp in milliseconds since device boot.
    pub time_ms: u32,
    /// Address of the tag string in ELF memory.
    pub tag_id: u32,
    /// Address of the format string in ELF memory.
    pub fmt_id: u32,
    /// Length of the payload following the header (or [`PAYLOAD_LEN_STREAMING`]).
    pub payload_len: u16,
}

impl Header {
    /// Parse an 18-byte little-endian header. Returns `None` on magic mismatch or
    /// unknown type/level nibbles.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        let magic = buf[0];
        if magic != PACKET_MAGIC {
            return None;
        }
        let type_level = buf[1];
        let ptype = PacketType::from_byte(type_level >> 4)?;
        let level = Level::from_byte(type_level & 0x0f)?;
        let seq = u16::from_le_bytes([buf[2], buf[3]]);
        let time_ms = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let tag_id = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let fmt_id = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        // payload_len occupies the final two bytes of the packed 18-byte header.
        let payload_len = u16::from_le_bytes([buf[16], buf[17]]);
        Some(Self {
            magic,
            ptype,
            level,
            seq,
            time_ms,
            tag_id,
            fmt_id,
            payload_len,
        })
    }
}
