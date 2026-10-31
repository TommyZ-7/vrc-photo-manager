use std::io::{self, BufRead, Read, Write};

use serde::{Serialize, de::DeserializeOwned};

/// Upper bound for one newline-delimited JSON message.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Writes `value` as one JSON line and flushes it.
pub fn write_message<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: Write,
    T: Serialize,
{
    let mut data = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if data.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC message is too large",
        ));
    }
    data.push(b'\n');
    writer.write_all(&data)?;
    writer.flush()
}

/// Reads one JSON line. Returns `None` at end of stream.
///
/// The read is capped at `MAX_MESSAGE_BYTES` plus the line terminator, so
/// an over-long line is rejected without buffering it in full.
pub fn read_message<R, T>(reader: &mut R) -> io::Result<Option<T>>
where
    R: BufRead,
    T: DeserializeOwned,
{
    loop {
        // Payload + "\r\n": two bytes past the cap are enough to tell an
        // over-long line apart from one that is exactly at the cap.
        let mut buf = Vec::new();
        let read = reader
            .by_ref()
            .take((MAX_MESSAGE_BYTES + 2) as u64)
            .read_until(b'\n', &mut buf)?;
        if read == 0 {
            return Ok(None);
        }
        while matches!(buf.last(), Some(b'\n' | b'\r')) {
            buf.pop();
        }
        if buf.len() > MAX_MESSAGE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC message is too large",
            ));
        }
        if buf.is_empty() {
            continue;
        }
        return serde_json::from_slice(&buf)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RequestEnvelope;

    #[test]
    fn framing_reads_one_newline_delimited_message() {
        let input = b"{\"id\":1,\"request\":{\"type\":\"status\"}}\n{\"id\":2,\"request\":{\"type\":\"shutdown\"}}\n";
        let mut reader = std::io::BufReader::new(&input[..]);
        let first: RequestEnvelope = read_message(&mut reader).unwrap().unwrap();
        let second: RequestEnvelope = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(first.id, 1);
        assert_eq!(second.id, 2);
        assert!(
            read_message::<_, RequestEnvelope>(&mut reader)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn framing_skips_empty_lines() {
        let input = b"\n\r\n{\"id\":1,\"request\":{\"type\":\"status\"}}\n";
        let mut reader = std::io::BufReader::new(&input[..]);
        let message: RequestEnvelope = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(message.id, 1);
    }

    #[test]
    fn framing_reads_a_final_message_without_a_newline() {
        let input = b"{\"id\":1,\"request\":{\"type\":\"status\"}}";
        let mut reader = std::io::BufReader::new(&input[..]);
        let message: RequestEnvelope = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(message.id, 1);
        assert!(
            read_message::<_, RequestEnvelope>(&mut reader)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn framing_rejects_a_line_over_the_cap() {
        let mut input = vec![b'x'; MAX_MESSAGE_BYTES + 1];
        input.push(b'\n');
        let mut reader = std::io::BufReader::new(&input[..]);
        let error = read_message::<_, RequestEnvelope>(&mut reader).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn framing_rejects_an_unterminated_line_over_the_cap() {
        // Far beyond the cap with no newline at all: the reader must stop at
        // the cap instead of buffering the whole stream.
        let input = vec![b'x'; MAX_MESSAGE_BYTES * 4];
        let mut reader = std::io::Cursor::new(input);
        let error = read_message::<_, RequestEnvelope>(&mut reader).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        // Only the capped prefix was consumed from the stream.
        assert_eq!(reader.position(), (MAX_MESSAGE_BYTES + 2) as u64);
    }

    #[test]
    fn framing_accepts_a_message_of_exactly_the_cap() {
        let overhead = "{\"pad\":\"\"}".len();
        let pad = "x".repeat(MAX_MESSAGE_BYTES - overhead);
        let input = format!("{{\"pad\":\"{pad}\"}}\n");
        assert_eq!(input.len(), MAX_MESSAGE_BYTES + 1);
        let mut reader = std::io::BufReader::new(input.as_bytes());
        let message: serde_json::Value = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(message["pad"].as_str().unwrap().len(), pad.len());
    }

    #[test]
    fn framing_round_trips_through_write_message() {
        let envelope = RequestEnvelope {
            id: 7,
            request: crate::protocol::Request::Status,
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &envelope).unwrap();
        let mut reader = std::io::BufReader::new(&bytes[..]);
        let read: RequestEnvelope = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(read.id, 7);
        assert!(matches!(read.request, crate::protocol::Request::Status));
    }
}
