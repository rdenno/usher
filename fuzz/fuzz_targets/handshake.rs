#![no_main]

use libfuzzer_sys::fuzz_target;
use usher_proto::handshake::{parse_hello_line, parse_ssh_connection};

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let _ = parse_hello_line(&s);
    let _ = parse_ssh_connection(&s);
});
