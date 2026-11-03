//! Newline-delimited JSON framing for the sidecar channel.

use std::io::{BufRead, Read, Write};

use serde::Serialize;

use crate::error::ProtocolError;
use crate::protocol::SidecarMessage;

/// Upper bound for one JSONL message, including the `hello` catalog.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Reads one JSONL message. Returns `None` at end of stream.
///
/// The read is capped just past [`MAX_LINE_BYTES`] so an over-long line is
/// rejected without buffering it in full. Blank lines are skipped, and both
/// `\n` and `\r\n` terminators are accepted.
pub fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<SidecarMessage>, ProtocolError> {
    loop {
        // Payload + "\r\n": two bytes past the cap are enough to tell an
        // over-long line apart from one that is exactly at the cap.
        let mut buf = Vec::new();
        let read = reader
            .by_ref()
            .take((MAX_LINE_BYTES + 2) as u64)
            .read_until(b'\n', &mut buf)?;
        if read == 0 {
            return Ok(None);
        }
        while matches!(buf.last(), Some(b'\n' | b'\r')) {
            buf.pop();
        }
        if buf.len() > MAX_LINE_BYTES {
            return Err(ProtocolError::LineTooLong);
        }
        if buf.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(&buf).map_err(|_| ProtocolError::InvalidUtf8)?;
        return serde_json::from_str(text)
            .map(Some)
            .map_err(ProtocolError::Json);
    }
}

/// Writes `value` as one JSON line and flushes it.
pub fn write_message<W: Write>(
    writer: &mut W,
    value: &impl Serialize,
) -> Result<(), ProtocolError> {
    let mut data = serde_json::to_vec(value)?;
    if data.len() > MAX_LINE_BYTES {
        return Err(ProtocolError::LineTooLong);
    }
    data.push(b'\n');
    writer.write_all(&data)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};

    use super::*;
    use crate::protocol::SidecarMessage;

    #[test]
    fn read_message_rejects_a_line_over_one_mebibyte() {
        let mut reader = BufReader::new(Cursor::new(vec![b'x'; MAX_LINE_BYTES + 2]));
        let error = read_message(&mut reader).unwrap_err();
        assert!(matches!(error, ProtocolError::LineTooLong));
    }

    #[test]
    fn read_message_accepts_a_line_at_the_cap() {
        let overhead = r#"{"type":"shutdown_ack","pad":""}"#.len();
        let pad = "x".repeat(MAX_LINE_BYTES - overhead);
        let input = format!("{{\"type\":\"shutdown_ack\",\"pad\":\"{pad}\"}}\n");
        assert_eq!(input.len(), MAX_LINE_BYTES + 1);
        let mut reader = BufReader::new(Cursor::new(input.into_bytes()));
        let message = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(message, SidecarMessage::ShutdownAck);
    }

    #[test]
    fn read_message_strips_lf_and_crlf_and_stops_at_eof() {
        let input = b"{\"type\":\"shutdown_ack\"}\r\n{\"type\":\"shutdown_ack\"}\n";
        let mut reader = BufReader::new(Cursor::new(input.to_vec()));
        assert_eq!(
            read_message(&mut reader).unwrap(),
            Some(SidecarMessage::ShutdownAck)
        );
        assert_eq!(
            read_message(&mut reader).unwrap(),
            Some(SidecarMessage::ShutdownAck)
        );
        assert_eq!(read_message(&mut reader).unwrap(), None);
    }

    #[test]
    fn read_message_skips_blank_lines() {
        let input = b"\n\r\n{\"type\":\"shutdown_ack\"}\n";
        let mut reader = BufReader::new(Cursor::new(input.to_vec()));
        assert_eq!(
            read_message(&mut reader).unwrap(),
            Some(SidecarMessage::ShutdownAck)
        );
    }

    #[test]
    fn read_message_rejects_invalid_utf8() {
        let mut reader = BufReader::new(Cursor::new(vec![0xff, b'\n']));
        let error = read_message(&mut reader).unwrap_err();
        assert!(matches!(error, ProtocolError::InvalidUtf8));
    }

    #[test]
    fn read_message_rejects_invalid_json() {
        let mut reader = BufReader::new(Cursor::new(b"not json\n".to_vec()));
        let error = read_message(&mut reader).unwrap_err();
        assert!(matches!(error, ProtocolError::Json(_)));
    }

    #[test]
    fn write_message_terminates_the_line_with_a_newline() {
        let mut bytes = Vec::new();
        write_message(&mut bytes, &SidecarMessage::ShutdownAck).unwrap();
        assert_eq!(bytes, b"{\"type\":\"shutdown_ack\"}\n");
    }

    #[test]
    fn write_message_rejects_a_line_over_one_mebibyte() {
        let mut bytes = Vec::new();
        let too_long = "x".repeat(MAX_LINE_BYTES);
        let error = write_message(&mut bytes, &too_long).unwrap_err();
        assert!(matches!(error, ProtocolError::LineTooLong));
        assert!(bytes.is_empty());
    }
}
