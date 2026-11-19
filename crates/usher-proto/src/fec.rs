use anyhow::{Result, bail};

pub const MAX_PARITY_SHARDS: usize = 16;

/// Computes the number of XOR parity stripes for `data_count` data shards
/// given `fec_percent` redundancy (`0..=50`).
pub fn compute_parity_count(data_count: usize, fec_percent: u8) -> usize {
    if fec_percent == 0 || data_count == 0 {
        return 0;
    }
    let clamped = fec_percent.min(50) as usize;
    let raw = (data_count * clamped).div_ceil(100);
    raw.clamp(1, MAX_PARITY_SHARDS)
}

/// Generates striped XOR parity shards.
/// Each data shard is prefixed with a 2-byte little-endian original length
/// before XORing across its stripe (`idx % parity_count`), enabling exact
/// recovery even when the final data shard in a frame is shorter than the rest.
pub fn encode_parity_shards(data_shards: &[&[u8]], parity_count: usize) -> Result<Vec<Vec<u8>>> {
    if parity_count == 0 {
        return Ok(Vec::new());
    }
    if parity_count > MAX_PARITY_SHARDS {
        bail!("parity_count {parity_count} exceeds maximum {MAX_PARITY_SHARDS}");
    }

    let mut parity_bufs = vec![Vec::new(); parity_count];
    for (idx, shard) in data_shards.iter().enumerate() {
        let len_u16 = u16::try_from(shard.len())
            .map_err(|_| anyhow::anyhow!("data shard exceeds 65535 bytes"))?;
        let stripe = idx % parity_count;
        let buf = &mut parity_bufs[stripe];
        let needed = 2 + shard.len();
        if buf.len() < needed {
            buf.resize(needed, 0);
        }
        let len_bytes = len_u16.to_le_bytes();
        buf[0] ^= len_bytes[0];
        buf[1] ^= len_bytes[1];
        for (dst, &src) in buf[2..needed].iter_mut().zip(shard.iter()) {
            *dst ^= src;
        }
    }

    Ok(parity_bufs)
}

/// Attempts to reconstruct missing data shards from received XOR parity stripes.
/// Returns the number of missing data shards recovered.
pub fn recover_missing_shards(
    data_slots: &mut [Option<Vec<u8>>],
    parity_slots: &[Option<Vec<u8>>],
) -> usize {
    let parity_count = parity_slots.len();
    if parity_count == 0 || data_slots.is_empty() {
        return 0;
    }

    let mut recovered = 0usize;
    for (stripe, parity_opt) in parity_slots.iter().enumerate() {
        let Some(parity) = parity_opt else {
            continue;
        };
        if parity.len() < 2 {
            continue;
        }

        let mut missing_idx = None;
        let mut missing_count = 0usize;
        let mut idx = stripe;
        while idx < data_slots.len() {
            if data_slots[idx].is_none() {
                missing_idx = Some(idx);
                missing_count += 1;
                if missing_count > 1 {
                    break;
                }
            }
            idx += parity_count;
        }

        if missing_count != 1 {
            continue;
        }
        let target_idx = missing_idx.expect("missing_count == 1");

        let mut acc = parity.clone();
        let mut corrupt = false;
        let mut j = stripe;
        while j < data_slots.len() {
            if j != target_idx
                && let Some(shard) = &data_slots[j]
            {
                let Ok(len_u16) = u16::try_from(shard.len()) else {
                    corrupt = true;
                    break;
                };
                if 2 + shard.len() > acc.len() {
                    corrupt = true;
                    break;
                }
                let len_bytes = len_u16.to_le_bytes();
                acc[0] ^= len_bytes[0];
                acc[1] ^= len_bytes[1];
                for (dst, &src) in acc[2..2 + shard.len()].iter_mut().zip(shard.iter()) {
                    *dst ^= src;
                }
            }
            j += parity_count;
        }

        if corrupt {
            continue;
        }

        let orig_len = u16::from_le_bytes([acc[0], acc[1]]) as usize;
        if 2 + orig_len > acc.len() {
            continue;
        }

        data_slots[target_idx] = Some(acc[2..2 + orig_len].to_vec());
        recovered += 1;
    }

    recovered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_short_trailing_shard_and_burst_loss() {
        let shards: Vec<Vec<u8>> = (0..10)
            .map(|i| {
                let len = if i == 9 { 37 } else { 1150 };
                (0..len).map(|b| ((i * 31 + b) & 0xff) as u8).collect()
            })
            .collect();
        let refs: Vec<&[u8]> = shards.iter().map(Vec::as_slice).collect();
        // 30% FEC on 10 shards => 3 parity shards (stripes 0, 1, 2)
        let parity_count = compute_parity_count(refs.len(), 30);
        assert_eq!(parity_count, 3);
        let parities = encode_parity_shards(&refs, parity_count).unwrap();

        // Drop consecutive burst of 3 packets: shards 7, 8, and short trailing shard 9
        let mut slots: Vec<Option<Vec<u8>>> = shards.iter().cloned().map(Some).collect();
        slots[7] = None;
        slots[8] = None;
        slots[9] = None;
        let parity_slots: Vec<Option<Vec<u8>>> = parities.into_iter().map(Some).collect();

        let recovered = recover_missing_shards(&mut slots, &parity_slots);
        assert_eq!(recovered, 3);
        for (i, expected) in shards.iter().enumerate() {
            assert_eq!(slots[i].as_deref(), Some(expected.as_slice()));
        }
    }

    #[test]
    fn skips_stripe_with_two_missing_shards() {
        let shards: Vec<Vec<u8>> = (0..4).map(|i| vec![i as u8; 100]).collect();
        let refs: Vec<&[u8]> = shards.iter().map(Vec::as_slice).collect();
        let parities = encode_parity_shards(&refs, 1).unwrap();

        let mut slots: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
        slots[0] = None;
        slots[2] = None;
        let parity_slots: Vec<Option<Vec<u8>>> = parities.into_iter().map(Some).collect();

        assert_eq!(recover_missing_shards(&mut slots, &parity_slots), 0);
    }
}
