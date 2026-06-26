//! SQLite capture store for `on9log-capture`.
//!
//! The capture side writes raw transport outcomes (deframed on9log binary
//! frames, SLIP-framed plain text, raw plain text, and deframer diagnostics)
//! verbatim into SQLite, stamped with the host wall-clock millisecond at which
//! each outcome was received. The decode side replays them in capture order.
//!
//! Storage is intentionally **ELF-independent**: the customer, who does not have
//! the firmware ELF with the secret format strings, captures raw header+payload
//! bytes. Decoding to human-readable text happens later (the `decode`
//! subcommand) against the matching ELF, by re-running the same `Decoder` /
//! `CrashDecoder` pipeline the live `on9log` CLI uses.
//!
//! Each on9log frame stores its 18-byte header and payload as BLOBs; the parsed
//! header fields are also written as normal columns so a capture database can be
//! queried directly (`WHERE level = 1`, `ORDER BY seq`, ...) without re-parsing.

use std::path::Path;

use on9log_protocol::{Header, Outcome, RawFrame};
use rusqlite::{Connection, OpenFlags, Statement};

/// Convenience alias for fallible operations that can return any boxed error.
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// DDL statements that create the `meta` and `events` tables (plus indexes)
/// if they do not already exist. Executed at database open time.
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    captured_at_ms  INTEGER NOT NULL,
    kind            TEXT NOT NULL,
    seq             INTEGER,
    device_time_ms  INTEGER,
    level           INTEGER,
    ptype           INTEGER,
    tag_id          INTEGER,
    fmt_id          INTEGER,
    payload_len     INTEGER,
    header          BLOB,
    payload         BLOB,
    detail          TEXT
);
CREATE INDEX IF NOT EXISTS idx_events_captured ON events(captured_at_ms);
CREATE INDEX IF NOT EXISTS idx_events_seq ON events(seq);
";

/// Prepared INSERT statement for storing one event row.
const INSERT: &str = "\
INSERT INTO events (
    captured_at_ms, kind, seq, device_time_ms, level, ptype,
    tag_id, fmt_id, payload_len, header, payload, detail
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)";

/// Prepared SELECT statement that reads all event columns in capture order.
const SELECT_ALL: &str = "\
SELECT id, captured_at_ms, kind, seq, device_time_ms, level, ptype, tag_id,
       fmt_id, payload_len, header, payload, detail
FROM events ORDER BY id";

/// A SQLite database holding captured on9log transport outcomes.
pub struct CaptureDb {
    /// Underlying SQLite connection handle.
    conn: Connection,
}

impl CaptureDb {
    /// Open (creating if missing) a capture database and ensure its schema.
    /// Existing events are preserved, so capturing can append to a prior file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(Self { conn })
    }

    /// Open an existing capture database for reading. Errors if the file is
    /// missing or is not a capture database (missing `events` table). The
    /// connection is opened with SQLite read-only flags and `query_only` so
    /// decode can never mutate a customer's capture.
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(format!("capture database not found: {}", path.display()).into());
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        if !has_events_table(&conn)? {
            return Err(format!(
                "not an on9log-capture database (no `events` table): {}",
                path.display()
            )
            .into());
        }
        conn.pragma_update(None, "query_only", true)?;
        Ok(Self { conn })
    }

    /// Record a key/value capture-session fact (port, baud, start time, ...).
    pub fn write_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Store a batch of outcomes stamped with their per-event receive times, in
    /// one transaction for durability and throughput.
    pub fn store_outcomes(&mut self, stamped: &[(Outcome, u64)]) -> Result<()> {
        if stamped.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(INSERT)?;
            for (outcome, ts) in stamped {
                store_one(&mut stmt, outcome, *ts)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Total number of captured events.
    pub fn event_count(&self) -> Result<i64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        Ok(count)
    }

    /// Replay captured events in capture order. `f` receives each reconstructed
    /// [`Outcome`] and its host receive timestamp. Rows whose stored on9log
    /// header can no longer be parsed are skipped with a warning (they were
    /// valid when captured; this only happens if the database was hand-edited).
    pub fn for_each_event<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(&Outcome, u64) -> Result<()>,
    {
        let mut stmt = self.conn.prepare(SELECT_ALL)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let captured_at_ms: i64 = row.get(1)?;
            let kind: String = row.get(2)?;
            let seq = row.get::<_, Option<i64>>(3)?;
            let _device_time_ms = row.get::<_, Option<i64>>(4)?;
            let _level = row.get::<_, Option<i64>>(5)?;
            let _ptype = row.get::<_, Option<i64>>(6)?;
            let _tag_id = row.get::<_, Option<i64>>(7)?;
            let _fmt_id = row.get::<_, Option<i64>>(8)?;
            let _payload_len = row.get::<_, Option<i64>>(9)?;
            let header = row.get::<_, Option<Vec<u8>>>(10)?;
            let payload = row.get::<_, Option<Vec<u8>>>(11)?;
            let detail = row.get::<_, Option<String>>(12)?;

            match reconstruct(&kind, &header, &payload, &detail) {
                Some(outcome) => f(&outcome, captured_at_ms as u64)?,
                None => eprintln!(
                    "on9log-capture: skipping unparseable event id={id} kind={kind} seq={seq:?}"
                ),
            }
        }
        Ok(())
    }
}

/// Insert a single [`Outcome`] into the `events` table using the prepared
/// statement. Dispatches on the outcome variant to set the correct `kind`
/// column and relevant field values.
fn store_one(stmt: &mut Statement<'_>, outcome: &Outcome, ts: u64) -> Result<()> {
    match outcome {
        Outcome::Frame(f) => {
            stmt.execute(rusqlite::params![
                ts as i64,
                "on9log",
                f.header.seq as i64,
                f.header.time_ms as i64,
                f.header.level as i64,
                f.header.ptype as i64,
                f.header.tag_id as i64,
                f.header.fmt_id as i64,
                f.header.payload_len as i64,
                f.header_bytes.as_slice(),
                f.payload.as_slice(),
                Option::<String>::None,
            ])?;
        }
        Outcome::PlainText(bytes) => {
            stmt.execute(rusqlite::params![
                ts as i64,
                "text",
                Option::<i64>::None,
                Option::<i64>::None,
                Option::<i64>::None,
                Option::<i64>::None,
                Option::<i64>::None,
                Option::<i64>::None,
                Option::<i64>::None,
                Option::<&[u8]>::None,
                bytes.as_slice(),
                Option::<String>::None,
            ])?;
        }
        Outcome::UnknownFrameType(t) => {
            store_error(stmt, ts, "unknown_frame_type", Some(format!("{t:02x}")))?;
        }
        Outcome::BadMagic => store_error(stmt, ts, "bad_magic", None)?,
        Outcome::CrcMismatch => store_error(stmt, ts, "crc_mismatch", None)?,
        Outcome::Truncated => store_error(stmt, ts, "truncated", None)?,
        Outcome::LengthMismatch => store_error(stmt, ts, "length_mismatch", None)?,
        Outcome::FrameTooLong => store_error(stmt, ts, "frame_too_long", None)?,
        Outcome::InvalidEscape => store_error(stmt, ts, "invalid_escape", None)?,
    }
    Ok(())
}

/// Insert a diagnostic/error outcome (bad magic, CRC mismatch, truncated,
/// etc.) into the `events` table. Only the `kind` and optional `detail`
/// columns are populated; frame-specific fields are all `NULL`.
fn store_error(
    stmt: &mut Statement<'_>,
    ts: u64,
    code: &str,
    detail: Option<String>,
) -> Result<()> {
    stmt.execute(rusqlite::params![
        ts as i64,
        code,
        Option::<i64>::None,
        Option::<i64>::None,
        Option::<i64>::None,
        Option::<i64>::None,
        Option::<i64>::None,
        Option::<i64>::None,
        Option::<i64>::None,
        Option::<&[u8]>::None,
        Option::<&[u8]>::None,
        detail,
    ])?;
    Ok(())
}

/// Reconstruct an [`Outcome`] from stored database columns.
///
/// The `kind` column determines which variant to use. For `"on9log"` frames
/// the header BLOB is re-parsed; for diagnostic kinds the string detail is
/// used when applicable. Returns `None` if the stored data cannot be parsed
/// (indicating a corrupted or hand-edited database).
fn reconstruct(
    kind: &str,
    header: &Option<Vec<u8>>,
    payload: &Option<Vec<u8>>,
    detail: &Option<String>,
) -> Option<Outcome> {
    match kind {
        "on9log" => {
            let header_bytes = header.clone()?;
            let header = Header::parse(&header_bytes)?;
            let payload = payload.clone().unwrap_or_default();
            Some(Outcome::Frame(RawFrame {
                header,
                header_bytes,
                payload,
            }))
        }
        "text" => Some(Outcome::PlainText(payload.clone().unwrap_or_default())),
        "bad_magic" => Some(Outcome::BadMagic),
        "crc_mismatch" => Some(Outcome::CrcMismatch),
        "truncated" => Some(Outcome::Truncated),
        "length_mismatch" => Some(Outcome::LengthMismatch),
        "frame_too_long" => Some(Outcome::FrameTooLong),
        "unknown_frame_type" => {
            let d = detail.as_ref()?;
            let t = u8::from_str_radix(d, 16).ok()?;
            Some(Outcome::UnknownFrameType(t))
        }
        "invalid_escape" => Some(Outcome::InvalidEscape),
        _ => None,
    }
}

/// Check whether the database contains an `events` table. Used by
/// `open_readonly()` to reject files that are not on9log capture databases.
fn has_events_table(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='events'",
        [],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use on9log_protocol::wire::{PACKET_MAGIC, PAYLOAD_LEN_STREAMING};
    use on9log_protocol::{ArgType, Level, PacketType};

    /// Build a minimal valid on9log LOG frame payload (one u32 arg).
    fn log_payload() -> Vec<u8> {
        let mut p = vec![1u8, ArgType::Bits32 as u8];
        p.extend_from_slice(&42u32.to_le_bytes());
        p
    }

    fn frame(seq: u16) -> (Outcome, RawFrame) {
        let mut header_bytes = Vec::with_capacity(18);
        header_bytes.push(PACKET_MAGIC);
        header_bytes.push(((PacketType::Log as u8) << 4) | (Level::Info as u8));
        header_bytes.extend_from_slice(&seq.to_le_bytes());
        header_bytes.extend_from_slice(&1000u32.to_le_bytes()); // time_ms
        header_bytes.extend_from_slice(&0x4000_0000u32.to_le_bytes()); // tag_id
        header_bytes.extend_from_slice(&0x4000_1000u32.to_le_bytes()); // fmt_id
        header_bytes.extend_from_slice(&PAYLOAD_LEN_STREAMING.to_le_bytes());
        let header = Header::parse(&header_bytes).unwrap();
        let payload = log_payload();
        let raw = RawFrame {
            header,
            header_bytes: header_bytes.clone(),
            payload: payload.clone(),
        };
        (Outcome::Frame(raw.clone()), raw)
    }

    #[test]
    fn round_trips_frame_text_and_errors() {
        let dir = std::env::temp_dir().join(format!(
            "on9log_capture_test_{}.sqlite",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&dir);
        let mut db = CaptureDb::open(&dir).unwrap();

        let (frame_outcome, raw) = frame(7);
        let text_outcome = Outcome::PlainText(b"I (1) boot: hello\n".to_vec());
        let crc_outcome = Outcome::CrcMismatch;
        let unknown_outcome = Outcome::UnknownFrameType(0x42);

        let stamped: Vec<(Outcome, u64)> = vec![
            (frame_outcome, 1_700_000_000_000u64),
            (text_outcome, 1_700_000_000_500),
            (crc_outcome, 1_700_000_000_900),
            (unknown_outcome, 1_700_000_001_000),
        ];
        db.store_outcomes(&stamped).unwrap();
        assert_eq!(db.event_count().unwrap(), 4);

        let mut seen: Vec<(String, u64)> = Vec::new();
        db.for_each_event(|o, ts| {
            let label = match o {
                Outcome::Frame(f) => {
                    assert_eq!(f.header.seq, 7);
                    assert_eq!(f.header.level, Level::Info);
                    assert_eq!(f.header.ptype, PacketType::Log);
                    assert_eq!(f.payload, raw.payload);
                    assert_eq!(f.header_bytes, raw.header_bytes);
                    "frame".to_string()
                }
                Outcome::PlainText(b) => {
                    assert_eq!(b, b"I (1) boot: hello\n");
                    "text".to_string()
                }
                Outcome::CrcMismatch => "crc".to_string(),
                Outcome::UnknownFrameType(t) => {
                    assert_eq!(*t, 0x42);
                    "unknown".to_string()
                }
                _ => "other".to_string(),
            };
            seen.push((label, ts));
            Ok(())
        })
        .unwrap();

        assert_eq!(
            seen,
            vec![
                ("frame".to_string(), 1_700_000_000_000),
                ("text".to_string(), 1_700_000_000_500),
                ("crc".to_string(), 1_700_000_000_900),
                ("unknown".to_string(), 1_700_000_001_000),
            ]
        );

        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn open_readonly_rejects_non_capture_db() {
        let dir = std::env::temp_dir().join(format!(
            "on9log_capture_notdb_{}.sqlite",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&dir);
        // Opening creates an empty SQLite file with no tables.
        let _ = Connection::open(&dir).unwrap();
        let err = CaptureDb::open_readonly(&dir);
        assert!(err.is_err());
        let _ = std::fs::remove_file(&dir);
    }
}
