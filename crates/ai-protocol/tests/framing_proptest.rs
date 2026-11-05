//! Property tests for the sidecar JSONL framing.
//!
//! The example-based tests in `src/framing.rs` pin the exact cap and the
//! `\n`/`\r\n` handling. These tests feed arbitrary UTF-8 error payloads
//! through the writer and reader, and check that over-limit lines are rejected
//! on both sides.

use std::io::{BufReader, Cursor};

use proptest::prelude::*;
use vrc_photon_ai_protocol::{
    MAX_LINE_BYTES, ProtocolError, SidecarError, SidecarErrorCode, SidecarMessage, read_message,
    write_message,
};

/// Bounded arbitrary Unicode text. The bound keeps one case far below the
/// framing cap even when every character serialises as `\uXXXX`.
fn arb_text() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..=2048).prop_map(|chars| chars.into_iter().collect())
}

fn arb_error_code() -> impl Strategy<Value = SidecarErrorCode> {
    prop_oneof![
        Just(SidecarErrorCode::ProtocolError),
        Just(SidecarErrorCode::ImageMissing),
        Just(SidecarErrorCode::ImageDecodeFailed),
        Just(SidecarErrorCode::ModelLoadFailed),
        Just(SidecarErrorCode::BackendUnavailable),
        Just(SidecarErrorCode::Busy),
        Just(SidecarErrorCode::OutOfMemory),
        Just(SidecarErrorCode::Cancelled),
        Just(SidecarErrorCode::Internal),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn jsonl_round_trips_arbitrary_utf8_error_messages(
        message in arb_text(),
        code in arb_error_code(),
        retryable in any::<bool>(),
    ) {
        let value = SidecarMessage::Fatal {
            error: SidecarError {
                code,
                message,
                retryable,
            },
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &value).unwrap();

        prop_assert_eq!(bytes.last(), Some(&b'\n'));
        prop_assert_eq!(
            bytes.iter().filter(|&&byte| byte == b'\n').count(),
            1
        );

        let mut reader = BufReader::new(Cursor::new(bytes));
        let decoded = read_message(&mut reader).unwrap().unwrap();
        prop_assert_eq!(decoded, value);
        prop_assert_eq!(read_message(&mut reader).unwrap(), None);
    }

    #[test]
    fn writer_rejects_payloads_over_the_cap(extra in 1usize..=1024) {
        let text = "x".repeat(MAX_LINE_BYTES + extra);
        let mut bytes = Vec::new();
        let error = write_message(&mut bytes, &text).unwrap_err();

        prop_assert!(matches!(error, ProtocolError::LineTooLong));
        prop_assert!(bytes.is_empty(), "a rejected message must not be written");
    }

    #[test]
    fn reader_rejects_lines_over_the_cap(extra in 1usize..=1024) {
        let mut bytes = vec![b'x'; MAX_LINE_BYTES + extra];
        bytes.push(b'\n');
        let mut reader = BufReader::new(Cursor::new(bytes));
        let error = read_message(&mut reader).unwrap_err();

        prop_assert!(matches!(error, ProtocolError::LineTooLong));
    }

    #[test]
    fn reader_rejects_non_utf8_lines(
        bytes in proptest::collection::vec(any::<u8>(), 1..=256)
    ) {
        prop_assume!(!bytes.contains(&b'\n') && !bytes.contains(&b'\r'));
        prop_assume!(std::str::from_utf8(&bytes).is_err());

        let mut reader = BufReader::new(Cursor::new(bytes));
        let error = read_message(&mut reader).unwrap_err();
        prop_assert!(matches!(error, ProtocolError::InvalidUtf8));
    }
}
