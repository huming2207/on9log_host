//! Decode verified frames into structured records, resolving format/tag
//! strings through an optional [`ElfStrings`] table.

use crate::elf_resolv::ElfStrings;
use crate::framer::RawFrame;
use crate::printf::{self, Arg};
use crate::wire::{
    ArgType, Header, ID_INLINE, ImageIdKind, Level, NULL_STRING_LEN, PACKET_MAGIC,
    WIRE_FORMAT_VERSION, reset_reason_name,
};

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

/// Result of comparing a BOOT packet's image ID with the loaded ELF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageCheck {
    /// The device sent no image ID (older firmware, or its port has none).
    NotReported,
    /// No ELF was loaded, so there is nothing to compare against.
    NoElf,
    /// The ELF has the same ID as the device: decoded text can be trusted.
    Match,
    /// The ELF has a different ID: decoded strings may be wrong.
    Mismatch {
        /// The loaded ELF's ID of the same kind.
        elf_id: Vec<u8>,
    },
    /// The ELF has no ID of the kind the device reported.
    Unavailable,
    /// The device reported an ID kind this decoder does not know.
    UnknownKind,
}

/// A decoded BOOT packet.
#[derive(Debug, Clone)]
pub struct BootRecord {
    pub meta: PacketMeta,
    /// Random number the device picked for this boot.
    pub boot_id: u32,
    /// `on9log_reset_reason_t`; see [`reset_reason_name`].
    pub reset_reason: u8,
    /// Device wire format version; `None` for pre-v1 firmware (5-byte BOOT).
    pub wire_version: Option<u8>,
    /// How the image ID was produced.
    pub image_id_kind: ImageIdKind,
    /// The device's firmware image ID (empty if none).
    pub image_id: Vec<u8>,
    /// Comparison of [`Self::image_id`] with the loaded ELF.
    pub image_check: ImageCheck,
}

impl BootRecord {
    /// True when output after this boot cannot be trusted: the ELF does not
    /// match the device, or the device speaks a newer wire format.
    pub fn is_problem(&self) -> bool {
        matches!(self.image_check, ImageCheck::Mismatch { .. })
            || self.wire_version.is_some_and(|v| v > WIRE_FORMAT_VERSION)
    }

    /// One-line human-readable description, shared by the CLI and capture tool.
    pub fn summary(&self) -> String {
        let reason = reset_reason_name(self.reset_reason);
        let wire = match self.wire_version {
            Some(v) if v > WIRE_FORMAT_VERSION => {
                format!(
                    "wire v{v} (newer than supported v{WIRE_FORMAT_VERSION}; decoding may be wrong)"
                )
            }
            Some(v) => format!("wire v{v}"),
            None => "pre-v1 firmware".to_string(),
        };
        let kind = self.image_id_kind.name();
        let image = match &self.image_check {
            ImageCheck::NotReported => "no image ID".to_string(),
            ImageCheck::NoElf => format!(
                "image {kind} {} (no ELF loaded to check)",
                hex(&self.image_id)
            ),
            ImageCheck::Match => format!("image {kind} {} matches the ELF", hex(&self.image_id)),
            ImageCheck::Mismatch { elf_id } => format!(
                "IMAGE MISMATCH: device {kind} {} but ELF has {}; decoded strings may be wrong",
                hex(&self.image_id),
                hex(elf_id)
            ),
            ImageCheck::Unavailable => {
                format!(
                    "image {kind} {} (the ELF has no {kind} to check)",
                    hex(&self.image_id)
                )
            }
            ImageCheck::UnknownKind => format!("image ID of unknown kind {}", hex(&self.image_id)),
        };
        format!(
            "--- device boot (seq {}, t={}ms): reset={reason}, boot_id=0x{:08x}, {wire}, {image} ---",
            self.meta.seq, self.meta.time_ms, self.boot_id
        )
    }
}

/// A decoded TIME_SYNC packet. It is not part of the sequence, so it carries
/// no gap information.
#[derive(Debug, Clone)]
pub struct TimeSyncRecord {
    /// Device time when the packet was built.
    pub time_ms: u32,
    /// Boot this time belongs to; matches [`BootRecord::boot_id`].
    pub boot_id: u32,
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
    /// A device boot. Gap tracking restarts here.
    Boot(BootRecord),
    /// A time-sync marker from a batching transport.
    TimeSync(TimeSyncRecord),
    /// A frame whose header parsed but whose payload could not be decoded.
    Malformed {
        meta: Option<PacketMeta>,
        reason: String,
    },
}

impl DecodedPacket {
    /// Sequence metadata, if this packet is part of the sequence.
    pub fn meta(&self) -> Option<&PacketMeta> {
        match self {
            Self::Log(r) => Some(&r.meta),
            Self::Buffer(r) => Some(&r.meta),
            Self::Dropped(r) => Some(&r.meta),
            Self::Boot(r) => Some(&r.meta),
            Self::TimeSync(_) => None,
            Self::Malformed { meta, .. } => meta.as_ref(),
        }
    }

    /// Packets missed just before this one, if any.
    pub fn gap(&self) -> Option<u32> {
        self.meta().and_then(|m| m.gap)
    }
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
        match h.ptype {
            // Built outside the sink path; its seq (always 0) is not part of the sequence.
            crate::wire::PacketType::TimeSync => return decode_time_sync(frame),
            // A new boot restarts the device's counter, so the previous seq means nothing.
            crate::wire::PacketType::Boot => self.reset(),
            _ => {}
        }
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
            crate::wire::PacketType::Boot => decode_boot(frame, elf, meta),
            crate::wire::PacketType::TimeSync => unreachable!("handled above"),
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
        let (inline, p) = match split_inline(&frame.header, &frame.payload) {
            Ok(split) => split,
            Err(reason) => {
                return DecodedPacket::Malformed {
                    meta: Some(meta),
                    reason,
                };
            }
        };
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

        let tag = inline.tag_or_resolve(elf, frame.header.tag_id);
        let inline_format = inline.format.as_deref().map(String::from_utf8_lossy);
        let format = inline_format
            .as_deref()
            .or_else(|| elf.and_then(|e| e.read_format(frame.header.fmt_id)));
        let message = match format {
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
        let (inline, p) = match split_inline(&frame.header, &frame.payload) {
            Ok(split) => split,
            Err(reason) => {
                return DecodedPacket::Malformed {
                    meta: Some(meta),
                    reason,
                };
            }
        };
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
            tag: inline.tag_or_resolve(elf, frame.header.tag_id),
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

/// Tag and format strings carried in the payload instead of by address.
struct InlineStrings {
    tag: Option<Vec<u8>>,
    format: Option<Vec<u8>>,
}

impl InlineStrings {
    /// The inline tag, or the tag resolved from the ELF by address.
    fn tag_or_resolve(&self, elf: Option<&ElfStrings>, tag_id: u32) -> String {
        match &self.tag {
            Some(tag) => String::from_utf8_lossy(tag).into_owned(),
            None => resolve_tag(elf, tag_id),
        }
    }
}

/// Split off the inline tag and format that precede the payload when
/// `tag_id` / `fmt_id` is [`ID_INLINE`]: the tag first, then the format, each
/// a `u32` length followed by that many bytes.
fn split_inline<'a>(
    header: &Header,
    payload: &'a [u8],
) -> Result<(InlineStrings, &'a [u8]), String> {
    let mut rest = payload;
    let mut take = |what: &str| -> Result<Vec<u8>, String> {
        let len =
            take_u32(&mut rest).map_err(|_| format!("inline {what} length truncated"))? as usize;
        if rest.len() < len {
            return Err(format!("inline {what} truncated"));
        }
        let (bytes, tail) = rest.split_at(len);
        rest = tail;
        Ok(bytes.to_vec())
    };
    let tag = if header.tag_id == ID_INLINE {
        Some(take("tag")?)
    } else {
        None
    };
    let format = if header.fmt_id == ID_INLINE {
        Some(take("format")?)
    } else {
        None
    };
    Ok((InlineStrings { tag, format }, rest))
}

/// Decode a BOOT packet and check its image ID against the ELF.
///
/// Layout: `u32 boot_id | u8 reset_reason` (pre-v1 firmware stops here) then
/// `u8 wire_version | u8 image_id_kind | u8 image_id_len | image_id`.
fn decode_boot(frame: &RawFrame, elf: Option<&ElfStrings>, meta: PacketMeta) -> DecodedPacket {
    let p = &frame.payload;
    let malformed = |reason: &str| DecodedPacket::Malformed {
        meta: Some(meta.clone()),
        reason: reason.into(),
    };
    if p.len() < 5 {
        return malformed("boot payload truncated");
    }
    let boot_id = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
    let reset_reason = p[4];
    let (wire_version, image_id_kind, image_id) = if p.len() == 5 {
        (None, ImageIdKind::None, Vec::new())
    } else {
        if p.len() < 8 {
            return malformed("boot payload truncated");
        }
        let id_len = usize::from(p[7]);
        if p.len() != 8 + id_len {
            return malformed("boot payload length mismatch");
        }
        (Some(p[5]), ImageIdKind::from_byte(p[6]), p[8..].to_vec())
    };

    let image_check = if image_id.is_empty() || image_id_kind == ImageIdKind::None {
        ImageCheck::NotReported
    } else if matches!(image_id_kind, ImageIdKind::Unknown(_)) {
        ImageCheck::UnknownKind
    } else {
        match elf {
            None => ImageCheck::NoElf,
            Some(elf) => match elf.image_id(image_id_kind) {
                None => ImageCheck::Unavailable,
                Some(elf_id) if elf_id == image_id.as_slice() => ImageCheck::Match,
                Some(elf_id) => ImageCheck::Mismatch {
                    elf_id: elf_id.to_vec(),
                },
            },
        }
    };

    DecodedPacket::Boot(BootRecord {
        meta,
        boot_id,
        reset_reason,
        wire_version,
        image_id_kind,
        image_id,
        image_check,
    })
}

/// Decode a TIME_SYNC packet: `u32 boot_id`, with the device time in the header.
fn decode_time_sync(frame: &RawFrame) -> DecodedPacket {
    let p = &frame.payload;
    if p.len() != 4 {
        return DecodedPacket::Malformed {
            meta: None,
            reason: "time_sync payload length mismatch".into(),
        };
    }
    DecodedPacket::TimeSync(TimeSyncRecord {
        time_ms: frame.header.time_ms,
        boot_id: u32::from_le_bytes([p[0], p[1], p[2], p[3]]),
    })
}

/// Lower-case hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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

    fn frame(ptype: PacketType, seq: u16, tag_id: u32, fmt_id: u32, payload: Vec<u8>) -> RawFrame {
        RawFrame {
            header: crate::wire::Header {
                magic: PACKET_MAGIC,
                ptype,
                level: Level::Info,
                seq,
                time_ms: 1000,
                tag_id,
                fmt_id,
                payload_len: payload.len() as u16,
            },
            header_bytes: vec![],
            payload,
        }
    }

    fn inline_str(s: &str) -> Vec<u8> {
        let mut v = (s.len() as u32).to_le_bytes().to_vec();
        v.extend_from_slice(s.as_bytes());
        v
    }

    fn one_u32_arg(v: u32) -> Vec<u8> {
        build_log_payload(&[ArgType::Bits32 as u8], &v.to_le_bytes())
    }

    #[test]
    fn decodes_inline_tag_and_format() {
        let mut dec = Decoder::new();

        // Inline tag, format by address (no ELF: address shown).
        let mut p = inline_str("stack-tag");
        p.extend(one_u32_arg(2));
        match dec.decode(&frame(PacketType::Log, 1, ID_INLINE, 0x4000_1000, p), None) {
            DecodedPacket::Log(l) => {
                assert_eq!(l.tag, "stack-tag");
                assert!(l.message.starts_with("<fmt @0x40001000>"), "{}", l.message);
            }
            other => panic!("expected Log, got {other:?}"),
        }

        // Inline format renders even without an ELF.
        let mut p = inline_str("runtime value=%d");
        p.extend(one_u32_arg(3));
        match dec.decode(&frame(PacketType::Log, 2, 0x3f00_0000, ID_INLINE, p), None) {
            DecodedPacket::Log(l) => {
                assert_eq!(l.tag, "@0x3f000000");
                assert_eq!(l.message, "runtime value=3");
            }
            other => panic!("expected Log, got {other:?}"),
        }

        // Both inline: tag first, then format.
        let mut p = inline_str("t");
        p.extend(inline_str("both=%u"));
        p.extend(one_u32_arg(4));
        match dec.decode(&frame(PacketType::Log, 3, ID_INLINE, ID_INLINE, p), None) {
            DecodedPacket::Log(l) => {
                assert_eq!((l.tag.as_str(), l.message.as_str()), ("t", "both=4"))
            }
            other => panic!("expected Log, got {other:?}"),
        }

        // Inline tag on a buffer packet.
        let mut p = inline_str("buf-tag");
        for v in [4u32, 0, 4] {
            p.extend(v.to_le_bytes());
        }
        p.extend([1, 2, 3, 4]);
        match dec.decode(&frame(PacketType::Buffer, 4, ID_INLINE, 0, p), None) {
            DecodedPacket::Buffer(b) => {
                assert_eq!(b.tag, "buf-tag");
                assert_eq!(b.bytes, [1, 2, 3, 4]);
            }
            other => panic!("expected Buffer, got {other:?}"),
        }

        // A cut-off inline string is malformed, not misread as arguments.
        let mut p = 50u32.to_le_bytes().to_vec();
        p.extend(b"short");
        assert!(matches!(
            dec.decode(&frame(PacketType::Log, 5, ID_INLINE, 0, p), None),
            DecodedPacket::Malformed { .. }
        ));
    }

    fn boot_payload(kind: u8, id: &[u8]) -> Vec<u8> {
        let mut p = 0x1234_abcdu32.to_le_bytes().to_vec();
        p.extend([9, crate::wire::WIRE_FORMAT_VERSION, kind, id.len() as u8]);
        p.extend(id);
        p
    }

    #[test]
    fn decodes_boot_and_checks_the_image_id() {
        use sha2::{Digest, Sha256};
        let exe = std::env::current_exe().unwrap();
        let elf = ElfStrings::from_path(&exe).unwrap();
        let sha: Vec<u8> = Sha256::digest(std::fs::read(&exe).unwrap()).to_vec();
        let mut dec = Decoder::new();

        match dec.decode(
            &frame(PacketType::Boot, 0, 0, 0, boot_payload(1, &sha)),
            Some(&elf),
        ) {
            DecodedPacket::Boot(b) => {
                assert_eq!(b.boot_id, 0x1234_abcd);
                assert_eq!(b.reset_reason, 9);
                assert_eq!(b.wire_version, Some(crate::wire::WIRE_FORMAT_VERSION));
                assert_eq!(b.image_id_kind, ImageIdKind::ElfSha256);
                assert_eq!(b.image_check, ImageCheck::Match);
                assert!(!b.is_problem());
                assert!(b.summary().contains("brownout"), "{}", b.summary());
            }
            other => panic!("expected Boot, got {other:?}"),
        }

        let mut wrong = sha.clone();
        wrong[0] ^= 1;
        match dec.decode(
            &frame(PacketType::Boot, 0, 0, 0, boot_payload(1, &wrong)),
            Some(&elf),
        ) {
            DecodedPacket::Boot(b) => {
                assert_eq!(
                    b.image_check,
                    ImageCheck::Mismatch {
                        elf_id: sha.clone()
                    }
                );
                assert!(b.is_problem());
                assert!(b.summary().contains("IMAGE MISMATCH"));
            }
            other => panic!("expected Boot, got {other:?}"),
        }

        match dec.decode(
            &frame(PacketType::Boot, 0, 0, 0, boot_payload(1, &sha)),
            None,
        ) {
            DecodedPacket::Boot(b) => assert_eq!(b.image_check, ImageCheck::NoElf),
            other => panic!("expected Boot, got {other:?}"),
        }

        // Pre-v1 firmware sent only boot_id and reset_reason.
        let legacy = 7u32.to_le_bytes().iter().copied().chain([1]).collect();
        match dec.decode(&frame(PacketType::Boot, 0, 0, 0, legacy), Some(&elf)) {
            DecodedPacket::Boot(b) => {
                assert_eq!(b.wire_version, None);
                assert_eq!(b.image_check, ImageCheck::NotReported);
            }
            other => panic!("expected Boot, got {other:?}"),
        }

        // A declared ID length that does not match the payload is malformed.
        let mut bad = boot_payload(1, &sha);
        bad.pop();
        assert!(matches!(
            dec.decode(&frame(PacketType::Boot, 0, 0, 0, bad), Some(&elf)),
            DecodedPacket::Malformed { .. }
        ));
    }

    #[test]
    fn boot_restarts_gap_tracking_and_time_sync_stays_out() {
        let dropped = |seq| frame(PacketType::Dropped, seq, 0, 0, 1u32.to_le_bytes().to_vec());
        let mut dec = Decoder::new();
        assert_eq!(dec.decode(&dropped(500), None).gap(), None);
        assert_eq!(dec.decode(&dropped(501), None).gap(), None);

        // TIME_SYNC (seq 0) neither reports nor causes a gap.
        let sync = frame(PacketType::TimeSync, 0, 0, 0, 5u32.to_le_bytes().to_vec());
        match dec.decode(&sync, None) {
            DecodedPacket::TimeSync(t) => assert_eq!((t.boot_id, t.time_ms), (5, 1000)),
            other => panic!("expected TimeSync, got {other:?}"),
        }
        assert_eq!(dec.decode(&dropped(502), None).gap(), None);

        // The device rebooted: seq restarts without a huge false gap.
        let boot = frame(PacketType::Boot, 0, 0, 0, boot_payload(0, &[]));
        assert_eq!(dec.decode(&boot, None).gap(), None);
        assert_eq!(dec.decode(&dropped(1), None).gap(), None);
        assert_eq!(dec.decode(&dropped(3), None).gap(), Some(1));
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
