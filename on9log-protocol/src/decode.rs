//! Decode verified frames into structured records, resolving format/tag
//! strings through an optional [`ElfStrings`] table.

use crate::elf_resolv::ElfStrings;
use crate::framer::RawFrame;
use crate::printf::{self, Arg};
use crate::wire::{ArgType, Level, NULL_STRING_LEN, PACKET_MAGIC};

/// Common per-packet metadata, including detected sequence gaps.
#[derive(Debug, Clone)]
pub struct PacketMeta {
    pub seq: u16,
    pub time_ms: u32,
    /// Number of packets missed between the previous received seq and this one,
    /// using wrapping arithmetic. `None` for the first packet.
    pub gap: Option<u32>,
}

/// A decoded log packet: tag and fully rendered message.
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub meta: PacketMeta,
    pub level: Level,
    pub tag: String,
    pub message: String,
}

/// A decoded buffer-dump packet.
#[derive(Debug, Clone)]
pub struct BufferRecord {
    pub meta: PacketMeta,
    pub level: Level,
    pub tag: String,
    pub total_len: u32,
    pub offset: u32,
    pub bytes: Vec<u8>,
}

/// A device-side dropped-packet notification.
#[derive(Debug, Clone)]
pub struct DroppedRecord {
    pub meta: PacketMeta,
    pub count: u32,
}

/// A fully decoded packet, dispatched by type from a raw frame.
#[derive(Debug, Clone)]
pub enum DecodedPacket {
    /// A decoded log message with tag and rendered message.
    Log(LogRecord),
    /// A decoded buffer-dump chunk.
    Buffer(BufferRecord),
    /// A device-side dropped-packet notification.
    Dropped(DroppedRecord),
    /// Time-sync / boot packets are not currently emitted by the firmware; their
    /// raw payload is surfaced for forward compatibility.
    Other {
        meta: PacketMeta,
        kind: &'static str,
        payload: Vec<u8>,
    },
    /// A frame whose header parsed but whose payload could not be decoded.
    Malformed {
        meta: Option<PacketMeta>,
        reason: String,
    },
}

/// Stateful decoder tracking the sequence counter for gap detection.
///
/// The [`Decoder`] wraps a sequence tracker used to compute the number of
/// packets missed between consecutive frames. It also contains the packet-type
/// dispatch logic that decodes log, buffer, and dropped frames.
pub struct Decoder {
    /// The sequence number of the last decoded frame, used for gap detection.
    last_seq: Option<u16>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// Create a new `Decoder` with no prior sequence state.
    pub fn new() -> Self {
        Self { last_seq: None }
    }

    /// Reset sequence tracking (e.g. after a device reboot).
    pub fn reset(&mut self) {
        self.last_seq = None;
    }

    /// Record a sequence number and return the gap from the last recorded
    /// sequence (wrapping arithmetic). Returns `None` for the first packet
    /// or when no gap is detected.
    fn record_seq(&mut self, seq: u16) -> Option<u32> {
        let gap = match self.last_seq {
            None => None,
            Some(last) => {
                let expected = last.wrapping_add(1);
                if seq == expected {
                    None
                } else {
                    Some(u32::from(seq.wrapping_sub(expected)))
                }
            }
        };
        self.last_seq = Some(seq);
        gap
    }

    /// Decode one verified frame. `elf` resolves `fmt_id`/`tag_id`; pass `None`
    /// when no ELF was supplied (addresses render as hex).
    pub fn decode(&mut self, frame: &RawFrame, elf: Option<&ElfStrings>) -> DecodedPacket {
        let h = &frame.header;
        let gap = self.record_seq(h.seq);
        let meta = PacketMeta {
            seq: h.seq,
            time_ms: h.time_ms,
            gap,
        };

        match h.ptype {
            crate::wire::PacketType::Log => self.decode_log(frame, elf, meta),
            crate::wire::PacketType::Buffer => self.decode_buffer(frame, elf, meta),
            crate::wire::PacketType::Dropped => self.decode_dropped(frame, meta),
            crate::wire::PacketType::TimeSync => DecodedPacket::Other {
                meta,
                kind: "time_sync",
                payload: frame.payload.clone(),
            },
            crate::wire::PacketType::Boot => DecodedPacket::Other {
                meta,
                kind: "boot",
                payload: frame.payload.clone(),
            },
        }
    }

    /// Decode a log-type packet: extract arg-type table, decode each argument,
    /// resolve the tag and format string from ELF, and render the message.
    fn decode_log(
        &self,
        frame: &RawFrame,
        elf: Option<&ElfStrings>,
        meta: PacketMeta,
    ) -> DecodedPacket {
        let p = &frame.payload;
        if p.is_empty() {
            return DecodedPacket::Malformed {
                meta: Some(meta),
                reason: "empty log payload".into(),
            };
        }
        let arg_count = p[0] as usize;
        if 1 + arg_count > p.len() {
            return DecodedPacket::Malformed {
                meta: Some(meta),
                reason: "arg type table truncated".into(),
            };
        }
        let type_bytes = &p[1..1 + arg_count];
        let mut body = &p[1 + arg_count..];

        let mut args: Vec<Arg> = Vec::with_capacity(arg_count);
        for &tb in type_bytes {
            let t = match ArgType::from_byte(tb) {
                Some(t) => t,
                None => {
                    return DecodedPacket::Malformed {
                        meta: Some(meta),
                        reason: format!("unknown arg type {:#x}", tb),
                    };
                }
            };
            let arg = match decode_arg(t, &mut body) {
                Ok(a) => a,
                Err(e) => {
                    return DecodedPacket::Malformed {
                        meta: Some(meta),
                        reason: e,
                    };
                }
            };
            args.push(arg);
        }

        let tag = resolve_tag(elf, frame.header.tag_id);
        let message = match elf.and_then(|e| e.read_format(frame.header.fmt_id)) {
            Some(fmt) => printf::render_format(fmt, &args),
            None => {
                // No format string available: show address and a compact arg dump.
                format!(
                    "<fmt @0x{:08x}> {}",
                    frame.header.fmt_id,
                    summarize_args(&args)
                )
            }
        };

        DecodedPacket::Log(LogRecord {
            meta,
            level: frame.header.level,
            tag,
            message,
        })
    }

    /// Decode a buffer-dump packet: extract total_len, offset, chunk_len, and bytes.
    fn decode_buffer(
        &self,
        frame: &RawFrame,
        elf: Option<&ElfStrings>,
        meta: PacketMeta,
    ) -> DecodedPacket {
        let p = &frame.payload;
        if p.len() < 12 {
            return DecodedPacket::Malformed {
                meta: Some(meta),
                reason: "buffer header truncated".into(),
            };
        }
        let total_len = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
        let offset = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
        let chunk_len = u32::from_le_bytes([p[8], p[9], p[10], p[11]]) as usize;
        if p.len() != 12 + chunk_len {
            return DecodedPacket::Malformed {
                meta: Some(meta),
                reason: "buffer payload length mismatch".into(),
            };
        }
        let bytes = p[12..12 + chunk_len].to_vec();
        DecodedPacket::Buffer(BufferRecord {
            meta,
            level: frame.header.level,
            tag: resolve_tag(elf, frame.header.tag_id),
            total_len,
            offset,
            bytes,
        })
    }

    /// Decode a dropped-packet notification: extract the dropped count (u32 LE).
    fn decode_dropped(&self, frame: &RawFrame, meta: PacketMeta) -> DecodedPacket {
        let p = &frame.payload;
        if p.len() < 4 {
            return DecodedPacket::Malformed {
                meta: Some(meta),
                reason: "dropped payload truncated".into(),
            };
        }
        if p.len() != 4 {
            return DecodedPacket::Malformed {
                meta: Some(meta),
                reason: "dropped payload length mismatch".into(),
            };
        }
        let count = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
        DecodedPacket::Dropped(DroppedRecord { meta, count })
    }
}

/// Decode a single argument from the wire payload according to its [`ArgType`].
///
/// Advances the `body` slice past the consumed bytes. Returns an error if
/// the payload is truncated for the expected argument size.
fn decode_arg(t: ArgType, body: &mut &[u8]) -> Result<Arg, String> {
    match t {
        ArgType::Bits32 => {
            let v = take_u32(body)?;
            Ok(Arg::U32(v))
        }
        ArgType::Bits64 => {
            let v = take_u64(body)?;
            Ok(Arg::U64(v))
        }
        ArgType::Pointer => {
            let v = take_u32(body)?;
            Ok(Arg::Ptr(v))
        }
        ArgType::DynamicString | ArgType::DynamicStringView => {
            let len = take_u32(body)?;
            if len == NULL_STRING_LEN {
                return Ok(Arg::Str(None));
            }
            let len = len as usize;
            if body.len() < len {
                return Err("dynamic string truncated".into());
            }
            let s = body[..len].to_vec();
            *body = &body[len..];
            Ok(Arg::Str(Some(s)))
        }
        ArgType::None => Ok(Arg::U32(0)),
    }
}

/// Read a little-endian `u32` from the front of the byte slice, advancing it.
fn take_u32(b: &mut &[u8]) -> Result<u32, String> {
    if b.len() < 4 {
        return Err("truncated u32".into());
    }
    let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    *b = &b[4..];
    Ok(v)
}

/// Read a little-endian `u64` from the front of the byte slice, advancing it.
fn take_u64(b: &mut &[u8]) -> Result<u64, String> {
    if b.len() < 8 {
        return Err("truncated u64".into());
    }
    let v = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
    *b = &b[8..];
    Ok(v)
}

/// Resolve a tag address to a human-readable string via ELF, falling back
/// to a hex address when the ELF is unavailable or the address is unknown.
fn resolve_tag(elf: Option<&ElfStrings>, addr: u32) -> String {
    match elf {
        Some(e) => match e.read_tag(addr) {
            Some(s) => s.to_string(),
            None => format!("@0x{:08x}", addr),
        },
        None => format!("@0x{:08x}", addr),
    }
}

/// Build a compact, human-readable summary of decoded arguments (used when
/// the format string itself is unavailable).
fn summarize_args(args: &[Arg]) -> String {
    let parts: Vec<String> = args
        .iter()
        .map(|a| match a {
            Arg::U32(v) => format!("u32:{v}"),
            Arg::U64(v) => format!("u64:{v}"),
            Arg::Ptr(v) => format!("ptr:0x{v:08x}"),
            Arg::Str(Some(s)) => format!("str:{}", String::from_utf8_lossy(s)),
            Arg::Str(None) => "str:null".into(),
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

/// Sanity helper exposed for the CLI: validate a frame's magic byte.
pub fn frame_magic_ok(frame: &RawFrame) -> bool {
    frame.header.magic == PACKET_MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::PacketType;

    fn build_log_payload(arg_types: &[u8], body: &[u8]) -> Vec<u8> {
        let mut p = vec![arg_types.len() as u8];
        p.extend_from_slice(arg_types);
        p.extend_from_slice(body);
        p
    }

    #[test]
    fn decodes_simple_log_without_elf() {
        // fmt_id/tag_id unresolved; payload: one u32 arg.
        let payload = build_log_payload(&[ArgType::Bits32 as u8], &42u32.to_le_bytes());
        let header = crate::wire::Header {
            magic: PACKET_MAGIC,
            ptype: PacketType::Log,
            level: Level::Info,
            seq: 5,
            time_ms: 1000,
            tag_id: 0x4000_0000,
            fmt_id: 0x4000_1000,
            payload_len: 0xffff,
        };
        let frame = RawFrame {
            header,
            header_bytes: vec![],
            payload,
        };
        let mut dec = Decoder::new();
        match dec.decode(&frame, None) {
            DecodedPacket::Log(l) => {
                assert_eq!(l.level, Level::Info);
                assert_eq!(l.tag, "@0x40000000");
                assert!(l.message.starts_with("<fmt @0x40001000>"));
                assert!(l.message.contains("u32:42"));
            }
            other => panic!("expected Log, got {other:?}"),
        }
    }

    #[test]
    fn decodes_dynamic_string_view_as_string() {
        let mut body = Vec::new();
        body.extend_from_slice(&10u32.to_le_bytes());
        body.extend_from_slice(b"hello\0view");
        let payload = build_log_payload(&[ArgType::DynamicStringView as u8], &body);
        let header = crate::wire::Header {
            magic: PACKET_MAGIC,
            ptype: PacketType::Log,
            level: Level::Info,
            seq: 1,
            time_ms: 1000,
            tag_id: 0x4000_0000,
            fmt_id: 0x4000_1000,
            payload_len: 0xffff,
        };
        let frame = RawFrame {
            header,
            header_bytes: vec![],
            payload,
        };
        let mut dec = Decoder::new();
        match dec.decode(&frame, None) {
            DecodedPacket::Log(l) => {
                assert!(l.message.starts_with("<fmt @0x40001000>"));
                assert!(l.message.contains("str:hello\0view"));
            }
            other => panic!("expected Log, got {other:?}"),
        }
    }

    #[test]
    fn detects_seq_gap() {
        let mk = |seq: u16| crate::wire::Header {
            magic: PACKET_MAGIC,
            ptype: PacketType::Dropped,
            level: Level::None,
            seq,
            time_ms: 0,
            tag_id: 0,
            fmt_id: 0,
            payload_len: 0xffff,
        };
        let mut dec = Decoder::new();
        let f1 = RawFrame {
            header: mk(1),
            header_bytes: vec![],
            payload: 1u32.to_le_bytes().to_vec(),
        };
        let f2 = RawFrame {
            header: mk(4),
            header_bytes: vec![],
            payload: 1u32.to_le_bytes().to_vec(),
        };
        let _ = dec.decode(&f1, None);
        match dec.decode(&f2, None) {
            DecodedPacket::Dropped(d) => assert_eq!(d.meta.gap, Some(2)),
            other => panic!("expected Dropped, got {other:?}"),
        }
    }

    #[test]
    fn rejects_truncated_buffer_payload() {
        let header = crate::wire::Header {
            magic: PACKET_MAGIC,
            ptype: PacketType::Buffer,
            level: Level::Info,
            seq: 1,
            time_ms: 0,
            tag_id: 0,
            fmt_id: 0,
            payload_len: 0xffff,
        };
        let mut payload = Vec::new();
        payload.extend_from_slice(&8u32.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&4u32.to_le_bytes());
        payload.extend_from_slice(&[1, 2]);
        let frame = RawFrame {
            header,
            header_bytes: vec![],
            payload,
        };

        let mut dec = Decoder::new();
        match dec.decode(&frame, None) {
            DecodedPacket::Malformed { reason, .. } => {
                assert_eq!(reason, "buffer payload length mismatch");
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn rejects_dropped_payload_with_extra_bytes() {
        let header = crate::wire::Header {
            magic: PACKET_MAGIC,
            ptype: PacketType::Dropped,
            level: Level::None,
            seq: 1,
            time_ms: 0,
            tag_id: 0,
            fmt_id: 0,
            payload_len: 4,
        };
        let mut payload = 1u32.to_le_bytes().to_vec();
        payload.push(0);
        let frame = RawFrame {
            header,
            header_bytes: vec![],
            payload,
        };

        let mut dec = Decoder::new();
        match dec.decode(&frame, None) {
            DecodedPacket::Malformed { reason, .. } => {
                assert_eq!(reason, "dropped payload length mismatch");
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }
}
