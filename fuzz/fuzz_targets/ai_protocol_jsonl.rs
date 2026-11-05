#![no_main]

use std::io::BufReader;

use libfuzzer_sys::fuzz_target;
use vrc_photon_ai_protocol::read_message;

fuzz_target!(|data: &[u8]| {
    let mut reader = BufReader::new(data);
    while let Ok(Some(_)) = read_message(&mut reader) {}
});
