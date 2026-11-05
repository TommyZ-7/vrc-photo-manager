//! Property tests for the newline-delimited JSON framing on the IPC channel.
//!
//! The example-based tests in `src/framing.rs` cover the exact cap and the
//! happy path. These tests exercise arbitrary UTF-8 payloads (quotes,
//! backslashes, line separators, and multi-byte characters) plus lines just
//! over the 1 MiB limit.

use std::io::{BufReader, Cursor};

use proptest::prelude::*;
use serde_json::{Value, json};
use vrc_photon_ipc::framing::{MAX_MESSAGE_BYTES, read_message, write_message};

/// Bounded arbitrary Unicode text. The character bound keeps one case far
/// below the framing cap even when every character serialises as `\uXXXX`.
fn arb_text() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..=2048).prop_map(|chars| chars.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn jsonl_round_trips_arbitrary_utf8_payloads(text in arb_text(), sequence in any::<u64>()) {
        let value = json!({ "text": text, "sequence": sequence });
        let mut bytes = Vec::new();
        write_message(&mut bytes, &value).unwrap();

        prop_assert_eq!(bytes.last(), Some(&b'\n'));
        // JSON escapes newlines inside strings, so the frame is exactly one
        // line even when the payload itself contains line separators.
        prop_assert_eq!(
            bytes.iter().filter(|&&byte| byte == b'\n').count(),
            1
        );

        let mut reader = BufReader::new(Cursor::new(bytes));
        let decoded: Value = read_message(&mut reader).unwrap().unwrap();
        prop_assert_eq!(decoded, value);
        prop_assert_eq!(read_message::<_, Value>(&mut reader).unwrap(), None);
    }

    #[test]
    fn writer_rejects_payloads_over_the_cap(extra in 1usize..=1024) {
        let text = "x".repeat(MAX_MESSAGE_BYTES + extra);
        let mut bytes = Vec::new();
        let error = write_message(&mut bytes, &text).unwrap_err();

        prop_assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        prop_assert!(error.to_string().contains("too large"));
        prop_assert!(bytes.is_empty(), "a rejected message must not be written");
    }

    #[test]
    fn reader_rejects_lines_over_the_cap(extra in 1usize..=1024) {
        let mut bytes = vec![b'x'; MAX_MESSAGE_BYTES + extra];
        bytes.push(b'\n');
        let mut reader = BufReader::new(Cursor::new(bytes));
        let error = read_message::<_, Value>(&mut reader).unwrap_err();

        prop_assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        prop_assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn reader_rejects_non_utf8_lines(
        bytes in proptest::collection::vec(any::<u8>(), 1..=256)
    ) {
        prop_assume!(!bytes.contains(&b'\n') && !bytes.contains(&b'\r'));
        prop_assume!(std::str::from_utf8(&bytes).is_err());

        let mut reader = BufReader::new(Cursor::new(bytes));
        let error = read_message::<_, Value>(&mut reader).unwrap_err();
        prop_assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
