#![no_main]

use libfuzzer_sys::fuzz_target;
use usher_proto::fragment::{FrameReassembler, VideoSubheader};
use usher_proto::packet::PacketKind;

fuzz_target!(|data: &[u8]| {
    let _ = VideoSubheader::decode_split(data);

    let mut reasm = FrameReassembler::new(4);
    let _ = reasm.push_packet(PacketKind::VideoData, data);
    if data.len() > 2 {
        let mid = data.len() / 2;
        let _ = reasm.push_packet(PacketKind::VideoData, &data[..mid]);
        let _ = reasm.push_packet(PacketKind::VideoFec, &data[mid..]);
    }
});
