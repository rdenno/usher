use usher_proto::clipboard::{ClipboardReassembler, packetize_clipboard_text};
use usher_proto::crypto::{Role, SessionCrypto};
use usher_proto::cursor::{CursorPacket, CursorShapeReassembler, packetize_cursor_shape};
use usher_proto::fec::{MAX_PARITY_SHARDS, encode_parity_shards, recover_missing_shards};
use usher_proto::fragment::{FramePacketizer, FrameReassembler, VideoSubheader};
use usher_proto::handshake::{parse_hello_line, parse_ssh_connection};
use usher_proto::input::InputEvent;
use usher_proto::packet::{ControlMessage, PacketKind, PingPayload, PongPayload, open_packet};

struct Xorshift64(u64);

impl Xorshift64 {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn fill_bytes(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            let n = chunk.len();
            chunk.copy_from_slice(&v[..n]);
        }
    }
}

#[test]
fn deterministic_malformed_input_stress_never_panics() {
    let mut rng = Xorshift64(0x5553_4845_525f_3034);
    let mut host_crypto = SessionCrypto::new(&[0x11u8; 32], Role::Host);
    let mut reasm = FrameReassembler::new(8);
    let mut clip = ClipboardReassembler::new();
    let mut cursor_reasm = CursorShapeReassembler::new();

    // Seed with valid packetized frames & clipboard chunks before mutating.
    let valid_shards = FramePacketizer::packetize(1, 1000, 1, b"valid-payload-bytes", 1200, 20)
        .expect("valid packetize");
    let valid_clip = packetize_clipboard_text(7, "hello clipboard stress", 256);
    let valid_cursor = packetize_cursor_shape(3, 8, 8, 1, 2, &[0xAAu8; 8 * 8 * 4], 128);

    let mut buf = vec![0u8; 2048];
    for iter in 0..2500usize {
        let len = (rng.next_u64() as usize) % 300;
        rng.fill_bytes(&mut buf[..len]);
        let slice = &buf[..len];

        let _ = open_packet(&mut host_crypto, slice);
        let _ = ControlMessage::decode(slice);
        let _ = PingPayload::decode(slice);
        let _ = PongPayload::decode(slice);
        let _ = VideoSubheader::decode_split(slice);
        let kind = if iter.is_multiple_of(2) {
            PacketKind::VideoData
        } else {
            PacketKind::VideoFec
        };
        let _ = reasm.push_packet(kind, slice);
        let _ = InputEvent::decode(slice);
        let _ = clip.push_chunk(slice);
        let _ = CursorPacket::decode(slice);
        let _ = cursor_reasm.push_chunk(slice);

        let text = String::from_utf8_lossy(slice);
        let _ = parse_hello_line(&text);
        let _ = parse_ssh_connection(&text);

        if iter < 256 && len >= 4 {
            let parity_count = (slice[0] as usize) % (MAX_PARITY_SHARDS + 1);
            let shards: Vec<&[u8]> = slice[1..].chunks(17).take(8).collect();
            if let Ok(parity) = encode_parity_shards(&shards, parity_count) {
                let mut d_slots: Vec<Option<Vec<u8>>> =
                    shards.iter().map(|s| Some(s.to_vec())).collect();
                let mut p_slots: Vec<Option<Vec<u8>>> = parity.into_iter().map(Some).collect();
                if !d_slots.is_empty() {
                    d_slots[0] = None;
                }
                if !p_slots.is_empty() && (iter & 1 == 1) {
                    p_slots[0] = None;
                }
                let _ = recover_missing_shards(&mut d_slots, &p_slots);
            }
        }

        // Also test single-byte bit-flipped valid packets.
        if let Some((k, orig)) = valid_shards.get(iter % valid_shards.len()) {
            let mut mutated = orig.clone();
            if !mutated.is_empty() {
                let idx = (rng.next_u64() as usize) % mutated.len();
                mutated[idx] ^= (rng.next_u64() as u8) | 1;
            }
            let _ = VideoSubheader::decode_split(&mutated);
            let _ = reasm.push_packet(*k, &mutated);
        }
        if let Some(orig) = valid_clip.get(iter % valid_clip.len()) {
            let mut mutated = orig.clone();
            if !mutated.is_empty() {
                let idx = (rng.next_u64() as usize) % mutated.len();
                mutated[idx] ^= 0xff;
            }
            let _ = clip.push_chunk(&mutated);
        }
        if let Some(orig) = valid_cursor.get(iter % valid_cursor.len()) {
            let mut mutated = orig.clone();
            if !mutated.is_empty() {
                let idx = (rng.next_u64() as usize) % mutated.len();
                mutated[idx] ^= 0x5a;
            }
            let _ = CursorPacket::decode(&mutated);
            let _ = cursor_reasm.push_chunk(&mutated);
        }
    }
}
