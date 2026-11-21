#![no_main]

use libfuzzer_sys::fuzz_target;
use usher_proto::clipboard::ClipboardReassembler;
use usher_proto::input::InputEvent;

fuzz_target!(|data: &[u8]| {
    let _ = InputEvent::decode(data);
    let mut reasm = ClipboardReassembler::new();
    let _ = reasm.push_chunk(data);
    if data.len() > 12 {
        let mid = data.len() / 2;
        let _ = reasm.push_chunk(&data[..mid]);
        let _ = reasm.push_chunk(&data[mid..]);
    }
});
