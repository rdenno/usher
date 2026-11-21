#![no_main]

use libfuzzer_sys::fuzz_target;
use usher_proto::crypto::{Role, SessionCrypto};
use usher_proto::packet::{ControlMessage, PingPayload, PongPayload, open_packet};

fuzz_target!(|data: &[u8]| {
    let mut crypto = SessionCrypto::new(&[0x42u8; 32], Role::Host);
    let _ = open_packet(&mut crypto, data);
    let _ = ControlMessage::decode(data);
    let _ = PingPayload::decode(data);
    let _ = PongPayload::decode(data);
});
