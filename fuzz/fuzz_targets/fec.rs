#![no_main]

use libfuzzer_sys::fuzz_target;
use usher_proto::fec::{MAX_PARITY_SHARDS, encode_parity_shards, recover_missing_shards};

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let parity_count = (data[0] as usize) % (MAX_PARITY_SHARDS + 1);
    let shard_count = ((data[1] as usize) % 16).max(1);
    let body = &data[2..];
    let chunk_size = (body.len() / shard_count).max(1);
    let shards: Vec<&[u8]> = body.chunks(chunk_size).take(shard_count).collect();
    if shards.is_empty() {
        return;
    }

    if let Ok(parity_raw) = encode_parity_shards(&shards, parity_count) {
        let mut data_slots: Vec<Option<Vec<u8>>> =
            shards.iter().map(|s| Some(s.to_vec())).collect();
        let mut parity_slots: Vec<Option<Vec<u8>>> = parity_raw.into_iter().map(Some).collect();
        if !data_slots.is_empty() {
            data_slots[0] = None;
        }
        if parity_slots.len() > 1 && data[2] & 1 == 1 {
            parity_slots[1] = None;
        }
        let _ = recover_missing_shards(&mut data_slots, &parity_slots);
    }
});
