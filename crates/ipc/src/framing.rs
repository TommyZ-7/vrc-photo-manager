use std::io::{self, BufRead, Write};

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
pub fn read_message<R, T>(reader: &mut R) -> io::Result<Option<T>>
where
    R: BufRead,
    T: DeserializeOwned,
{
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        if line.len() > MAX_MESSAGE_BYTES + 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC message is too large",
            ));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        return serde_json::from_str(line)
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
}
