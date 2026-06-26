//! Transport SLIP deframing + CRC verification.
//!
//! The ESP VFS transport wraps both on9log packets and plain-text stdio as:
//! `0xa5 SLIP(frame_type) SLIP(payload) SLIP(crc16_le) 0xc0`.
//! This module turns the raw UART byte stream into verified typed frames.

use crate::crc;
use crate::wire::{
    HEADER_LEN, Header, PAYLOAD_LEN_STREAMING, SLIP_END, SLIP_ESC, SLIP_ESC_CR, SLIP_ESC_END,
    SLIP_ESC_ESC, SLIP_ESC_LF, SLIP_ESC_START, SLIP_START, TRANSPORT_FRAME_ON9LOG,
    TRANSPORT_FRAME_TEXT, TRANSPORT_MAX_PAYLOAD,
};

/// A fully decoded on9log packet carried inside a CRC-verified transport frame.
#[derive(Debug, Clone)]
pub struct RawFrame {
    /// Parsed 18-byte packet header (magic, type, level, sequence, timestamps, IDs).
    pub header: Header,
    /// Raw header bytes as received over the wire (for CRC re-computation etc.).
    pub header_bytes: Vec<u8>,
    /// Payload bytes following the header (arg-type table + argument values).
    pub payload: Vec<u8>,
}

/// Outcome of feeding bytes: either a verified frame or a recoverable error
/// (bad magic, CRC mismatch, truncated). Errors do not halt decoding.
#[derive(Debug)]
pub enum Outcome {
    /// A successfully decoded and CRC-verified on9log packet.
    Frame(RawFrame),
    /// A plain-text chunk (stdout/stderr) carried in a text transport frame or
    /// raw bytes outside any SLIP frame.
    PlainText(Vec<u8>),
    /// The on9log packet magic byte was missing or wrong.
    BadMagic,
    /// The transport frame CRC-16 did not match the computed checksum.
    CrcMismatch,
    /// The transport frame was too short to contain even a type byte and CRC.
    Truncated,
    /// The on9log payload length field does not match the actual payload size
    /// (only checked for non-streaming frames).
    LengthMismatch,
    /// The accumulated transport frame exceeded [`TRANSPORT_MAX_PAYLOAD`].
    FrameTooLong,
    /// The transport frame type byte is neither on9log nor text.
    UnknownFrameType(u8),
    /// An invalid SLIP escape sequence was encountered.
    InvalidEscape,
}

/// Streaming transport deframer. Feed it arbitrary byte slices from the UART.
///
/// The deframer maintains state across calls to [`Deframer::feed`], handling
/// SLIP frame boundaries, escape sequences, CRC verification, and raw-text
/// accumulation. Each call returns a batch of [`Outcome`] values.
pub struct Deframer {
    /// Accumulated bytes for the current SLIP frame (after un-escaping).
    buf: Vec<u8>,
    /// Accumulated raw text bytes outside any SLIP frame.
    raw_text: Vec<u8>,
    /// `true` while inside a SLIP frame (between `SLIP_START` and `SLIP_END`).
    in_frame: bool,
    /// `true` when the previous byte was a SLIP escape (`SLIP_ESC`).
    escape: bool,
}

impl Default for Deframer {
    fn default() -> Self {
        Self::new()
    }
}

impl Deframer {
    /// Create a new `Deframer` with empty internal state.
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(256),
            raw_text: Vec::with_capacity(256),
            in_frame: false,
            escape: false,
        }
    }

    /// Feed a chunk of transport bytes; returns completed frame outcomes in
    /// arrival order.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Outcome> {
        let mut out = Vec::new();
        for &b in data {
            if !self.in_frame {
                if b == SLIP_START {
                    self.flush_raw_text(&mut out);
                    self.in_frame = true;
                    self.escape = false;
                    self.buf.clear();
                } else {
                    self.raw_text.push(b);
                    if b == b'\n' || self.raw_text.len() >= 256 {
                        self.flush_raw_text(&mut out);
                    }
                }
                continue;
            }

            if self.escape {
                let unescaped = match b {
                    SLIP_ESC_END => Some(SLIP_END),
                    SLIP_ESC_ESC => Some(SLIP_ESC),
                    SLIP_ESC_START => Some(SLIP_START),
                    SLIP_ESC_CR => Some(b'\r'),
                    SLIP_ESC_LF => Some(b'\n'),
                    _ => None,
                };
                if let Some(v) = unescaped {
                    self.push_byte(v, &mut out);
                } else {
                    self.buf.clear();
                    self.in_frame = false;
                    out.push(Outcome::InvalidEscape);
                }
                self.escape = false;
                continue;
            }

            match b {
                SLIP_START => {
                    // A fresh unescaped start marker is a reliable resync point.
                    self.buf.clear();
                    self.escape = false;
                }
                SLIP_END => {
                    if let Some(o) = Self::decode_frame(&self.buf) {
                        out.push(o);
                    }
                    self.buf.clear();
                    self.escape = false;
                    self.in_frame = false;
                }
                SLIP_ESC => self.escape = true,
                other => {
                    self.push_byte(other, &mut out);
                }
            }
        }
        self.flush_raw_text(&mut out);
        out
    }

    /// Push a single unescaped byte into the frame buffer, checking the maximum
    /// transport frame size. On overflow the accumulated buffer is discarded and
    /// [`Outcome::FrameTooLong`] is emitted.
    fn push_byte(&mut self, byte: u8, out: &mut Vec<Outcome>) {
        let max_frame_bytes = 1 + TRANSPORT_MAX_PAYLOAD + 2;
        if self.buf.len() >= max_frame_bytes {
            self.buf.clear();
            self.in_frame = false;
            self.escape = false;
            out.push(Outcome::FrameTooLong);
            return;
        }
        self.buf.push(byte);
    }

    /// Flush any accumulated raw text bytes as an [`Outcome::PlainText`].
    fn flush_raw_text(&mut self, out: &mut Vec<Outcome>) {
        if !self.raw_text.is_empty() {
            out.push(Outcome::PlainText(std::mem::take(&mut self.raw_text)));
        }
    }

    /// Decode one unescaped transport frame buffer into an `Outcome`.
    ///
    /// The buffer layout is: `type(1) | payload(N) | crc_le(2)`.
    /// This method validates the CRC, checks the payload length, and
    /// dispatches by frame type to the on9log or plain-text decoder.
    fn decode_frame(buf: &[u8]) -> Option<Outcome> {
        // type(1) + crc(2) is the minimum transport frame.
        if buf.len() < 3 {
            return Some(Outcome::Truncated);
        }
        let frame_type = buf[0];
        let crc_bytes: [u8; 2] = [buf[buf.len() - 2], buf[buf.len() - 1]];
        let payload = &buf[1..buf.len() - 2];
        if payload.len() > TRANSPORT_MAX_PAYLOAD {
            return Some(Outcome::FrameTooLong);
        }

        if !crc::verify(&buf[..1], payload, &crc_bytes) {
            return Some(Outcome::CrcMismatch);
        }

        match frame_type {
            TRANSPORT_FRAME_ON9LOG => Self::decode_on9log_payload(payload),
            TRANSPORT_FRAME_TEXT => Some(Outcome::PlainText(payload.to_vec())),
            other => Some(Outcome::UnknownFrameType(other)),
        }
    }

    /// Decode the inner on9log payload (after the transport frame type byte)
    /// into an [`Outcome::Frame`]. Validates the magic byte, parses the header,
    /// and checks the payload length against [`PAYLOAD_LEN_STREAMING`] or the
    /// declared length.
    fn decode_on9log_payload(payload: &[u8]) -> Option<Outcome> {
        // header(18) is the minimum inner on9log packet.
        if payload.len() < HEADER_LEN {
            return Some(Outcome::Truncated);
        }
        let header = match Header::parse(&payload[..HEADER_LEN]) {
            Some(h) => h,
            None => return Some(Outcome::BadMagic),
        };
        let inner_payload = &payload[HEADER_LEN..];
        if header.payload_len != PAYLOAD_LEN_STREAMING
            && inner_payload.len() != usize::from(header.payload_len)
        {
            return Some(Outcome::LengthMismatch);
        }

        Some(Outcome::Frame(RawFrame {
            header,
            header_bytes: payload[..HEADER_LEN].to_vec(),
            payload: inner_payload.to_vec(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{
        ArgType, HEADER_LEN, Level, PACKET_MAGIC, PAYLOAD_LEN_STREAMING, PacketType,
    };

    /// Build a full typed transport frame: `SLIP_START | esc(type) | esc(payload) | esc(crc_le) | SLIP_END`.
    fn build_transport_frame(frame_type: u8, payload: &[u8]) -> Vec<u8> {
        let crc = crate::crc::compute(&[frame_type], payload);
        let mut inner = Vec::new();
        inner.extend_from_slice(payload);
        inner.extend_from_slice(&crc.to_le_bytes());

        let mut out = vec![SLIP_START];
        push_slip(frame_type, &mut out);
        for &b in &inner {
            push_slip(b, &mut out);
        }
        out.push(SLIP_END);
        out
    }

    /// Build an on9log transport frame from raw header bytes and payload.
    fn build_on9log_frame(header: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut inner_payload = Vec::new();
        inner_payload.extend_from_slice(header);
        inner_payload.extend_from_slice(payload);
        build_transport_frame(TRANSPORT_FRAME_ON9LOG, &inner_payload)
    }

    /// Apply SLIP escaping to a single byte and append it to `out`.
    /// Escapes `SLIP_START`, `SLIP_END`, `SLIP_ESC`, `\r`, and `\n`.
    fn push_slip(byte: u8, out: &mut Vec<u8>) {
        match byte {
            SLIP_START => {
                out.push(SLIP_ESC);
                out.push(SLIP_ESC_START);
            }
            SLIP_END => {
                out.push(SLIP_ESC);
                out.push(SLIP_ESC_END);
            }
            SLIP_ESC => {
                out.push(SLIP_ESC);
                out.push(SLIP_ESC_ESC);
            }
            b'\r' => {
                out.push(SLIP_ESC);
                out.push(SLIP_ESC_CR);
            }
            b'\n' => {
                out.push(SLIP_ESC);
                out.push(SLIP_ESC_LF);
            }
            other => out.push(other),
        }
    }

    /// Build a minimal 18-byte on9log header with the given sequence number.
    fn make_header(seq: u16) -> Vec<u8> {
        let mut h = Vec::with_capacity(HEADER_LEN);
        h.push(PACKET_MAGIC);
        h.push(((PacketType::Log as u8) << 4) | (Level::Info as u8));
        h.extend_from_slice(&seq.to_le_bytes());
        h.extend_from_slice(&1000u32.to_le_bytes()); // time_ms
        h.extend_from_slice(&0x4000_0000u32.to_le_bytes()); // tag_id
        h.extend_from_slice(&0x4000_1000u32.to_le_bytes()); // fmt_id
        h.extend_from_slice(&PAYLOAD_LEN_STREAMING.to_le_bytes());
        h
    }

    #[test]
    fn deframes_full_slip_frame() {
        // payload: one u32 arg = 42
        let mut payload = vec![1u8, ArgType::Bits32 as u8];
        payload.extend_from_slice(&42u32.to_le_bytes());

        let header = make_header(7);
        let wire = build_on9log_frame(&header, &payload);

        let mut d = Deframer::new();
        let outcomes = d.feed(&wire);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            Outcome::Frame(f) => {
                assert_eq!(f.header.seq, 7);
                assert_eq!(f.header.level, Level::Info);
                assert_eq!(f.header.ptype, PacketType::Log);
                assert_eq!(f.payload, payload);
            }
            o => panic!("expected Frame, got {o:?}"),
        }
    }

    #[test]
    fn detects_crc_corruption() {
        let payload = vec![0u8]; // arg_count 0
        let header = make_header(1);
        let mut wire = build_on9log_frame(&header, &payload);
        // Flip an escaped-frame byte after START + type.
        let idx = 2 + HEADER_LEN;
        wire[idx] ^= 0xff;
        let mut d = Deframer::new();
        let outcomes = d.feed(&wire);
        assert!(matches!(outcomes[0], Outcome::CrcMismatch));
    }

    #[test]
    fn rejects_non_streaming_length_mismatch() {
        let mut payload = 1u32.to_le_bytes().to_vec();
        payload.push(0xff);
        let mut header = make_header(3);
        let len_offset = HEADER_LEN - 2;
        header[len_offset..HEADER_LEN].copy_from_slice(&4u16.to_le_bytes());
        let wire = build_on9log_frame(&header, &payload);

        let mut d = Deframer::new();
        let outcomes = d.feed(&wire);
        assert!(matches!(outcomes[0], Outcome::LengthMismatch));
    }

    #[test]
    fn handles_split_feeds_and_escape() {
        // Payload containing 0xc0 bytes to exercise SLIP escaping.
        let mut payload = vec![1u8, ArgType::Bits32 as u8];
        payload.extend_from_slice(&0xc0_c0_c0_c0u32.to_le_bytes());
        let header = make_header(2);
        let wire = build_on9log_frame(&header, &payload);

        // Feed in two chunks to verify incremental deframing.
        let split = wire.len() / 2;
        let mut d = Deframer::new();
        let mut outcomes = d.feed(&wire[..split]);
        outcomes.extend(d.feed(&wire[split..]));
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            Outcome::Frame(f) => assert_eq!(f.payload, payload),
            o => panic!("expected Frame, got {o:?}"),
        }
    }

    #[test]
    fn emits_plain_text_frame() {
        let wire = build_transport_frame(TRANSPORT_FRAME_TEXT, b"I (123) boot: hello\n");

        let mut d = Deframer::new();
        let outcomes = d.feed(&wire);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            Outcome::PlainText(text) => assert_eq!(text, b"I (123) boot: hello\n"),
            o => panic!("expected PlainText, got {o:?}"),
        }
    }

    #[test]
    fn text_newline_is_escaped_for_uart_vfs() {
        let wire = build_transport_frame(TRANSPORT_FRAME_TEXT, b"line\n");
        assert!(!wire[1..wire.len() - 1].contains(&b'\n'));

        let mut d = Deframer::new();
        let outcomes = d.feed(&wire);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            Outcome::PlainText(text) => assert_eq!(text, b"line\n"),
            o => panic!("expected PlainText, got {o:?}"),
        }
    }

    #[test]
    fn handles_text_before_on9log_frame() {
        let payload = vec![0u8];
        let header = make_header(4);
        let mut wire = build_transport_frame(TRANSPORT_FRAME_TEXT, b"I (123) boot: hello\n");
        wire.extend(build_on9log_frame(&header, &payload));

        let mut d = Deframer::new();
        let outcomes = d.feed(&wire);
        assert_eq!(outcomes.len(), 2);
        match &outcomes[0] {
            Outcome::PlainText(text) => assert_eq!(text, b"I (123) boot: hello\n"),
            o => panic!("expected PlainText, got {o:?}"),
        }
        assert!(matches!(outcomes[1], Outcome::Frame(_)));
    }

    #[test]
    fn preserves_plain_text_across_split_feeds() {
        let wire = build_transport_frame(TRANSPORT_FRAME_TEXT, b"I (1) tag: msg\n");
        let mut d = Deframer::new();
        let split = wire.len() / 2;
        let outcomes = d.feed(&wire[..split]);
        assert!(outcomes.is_empty());
        let outcomes = d.feed(&wire[split..]);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            Outcome::PlainText(text) => assert_eq!(text, b"I (1) tag: msg\n"),
            o => panic!("expected PlainText, got {o:?}"),
        }
    }

    #[test]
    fn emits_printable_text_outside_transport_frames() {
        let mut d = Deframer::new();
        let outcomes = d.feed(b"I (42) tag: text only\n");
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            Outcome::PlainText(text) => assert_eq!(text, b"I (42) tag: text only\n"),
            o => panic!("expected PlainText, got {o:?}"),
        }
    }

    #[test]
    fn preserves_arbitrary_raw_text_bytes_outside_transport_frames() {
        let mut d = Deframer::new();
        let outcomes = d.feed(&[0x00, 0x01, b'O', b'K', 0xff, b'\n']);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            Outcome::PlainText(text) => assert_eq!(text, &[0x00, 0x01, b'O', b'K', 0xff, b'\n']),
            o => panic!("expected PlainText, got {o:?}"),
        }
    }

    #[test]
    fn preserves_ansi_escape_bytes_outside_transport_frames() {
        let mut d = Deframer::new();
        let outcomes = d.feed(b"\x1b[0;32mI (42) tag: colored\x1b[0m\n");
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            Outcome::PlainText(text) => {
                assert_eq!(text, b"\x1b[0;32mI (42) tag: colored\x1b[0m\n")
            }
            o => panic!("expected PlainText, got {o:?}"),
        }
    }

    #[test]
    fn resyncs_after_missing_start_marker() {
        let payload = vec![0u8];
        let header = make_header(5);
        let first = build_on9log_frame(&header, &payload);
        let second = build_on9log_frame(&make_header(6), &payload);
        let mut wire = first[1..].to_vec();
        wire.extend(second);

        let mut d = Deframer::new();
        let outcomes = d.feed(&wire);
        assert!(!outcomes.is_empty());
        match outcomes.iter().find(|o| matches!(o, Outcome::Frame(_))) {
            Some(Outcome::Frame(f)) => assert_eq!(f.header.seq, 6),
            Some(o) => panic!("expected Frame, got {o:?}"),
            None => panic!("expected recovered Frame, got {outcomes:?}"),
        }
    }

    #[test]
    fn rejects_oversized_transport_frame() {
        let mut wire = vec![SLIP_START];
        wire.extend(std::iter::repeat_n(0x55, TRANSPORT_MAX_PAYLOAD + 4));
        let mut d = Deframer::new();
        let outcomes = d.feed(&wire);
        assert!(matches!(outcomes[0], Outcome::FrameTooLong));
    }

    #[test]
    fn rejects_invalid_escape_sequence() {
        let wire = [SLIP_START, SLIP_ESC, SLIP_END];
        let mut d = Deframer::new();
        let outcomes = d.feed(&wire);
        assert!(matches!(outcomes[0], Outcome::InvalidEscape));
    }
}
