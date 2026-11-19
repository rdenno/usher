use crate::fec::{MAX_PARITY_SHARDS, compute_parity_count, encode_parity_shards, recover_missing_shards};
use crate::packet::{PacketKind, max_plaintext_len};
use anyhow::{Result, bail};
use std::collections::BTreeMap;

/// Subheader prepended inside the encrypted plaintext of `VideoData` and `VideoFec` packets:
/// `[frame_id: 4B LE][packet_idx: 2B LE][data_shards: 2B LE][parity_shards: 1B][frame_flags: 1B][timestamp_us: 4B LE]`
pub const VIDEO_SUBHEADER_LEN: usize = 14;
pub const FRAME_FLAG_KEYFRAME: u8 = 0x01;
pub const MAX_DATA_SHARDS_PER_FRAME: usize = 4096;

/// Maximum raw payload chunk inside one `VideoData` packet so that BOTH
/// `VideoData` and `VideoFec` (with its 2-byte length prefix) fit inside `mtu`
/// after adding the outer 12B envelope, 16B AEAD tag, and 14B video subheader.
pub const fn max_video_chunk_len(mtu: usize) -> usize {
    max_plaintext_len(mtu).saturating_sub(VIDEO_SUBHEADER_LEN + 2)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSubheader {
    pub frame_id: u32,
    pub packet_idx: u16,
    pub data_shards: u16,
    pub parity_shards: u8,
    pub flags: u8,
    pub timestamp_us: u32,
}

impl VideoSubheader {
    pub fn encode(&self) -> [u8; VIDEO_SUBHEADER_LEN] {
        let mut out = [0u8; VIDEO_SUBHEADER_LEN];
        out[0..4].copy_from_slice(&self.frame_id.to_le_bytes());
        out[4..6].copy_from_slice(&self.packet_idx.to_le_bytes());
        out[6..8].copy_from_slice(&self.data_shards.to_le_bytes());
        out[8] = self.parity_shards;
        out[9] = self.flags;
        out[10..14].copy_from_slice(&self.timestamp_us.to_le_bytes());
        out
    }

    pub fn decode_split(bytes: &[u8]) -> Result<(Self, &[u8])> {
        if bytes.len() < VIDEO_SUBHEADER_LEN {
            bail!("video plaintext shorter than {VIDEO_SUBHEADER_LEN}B subheader");
        }
        let hdr = Self {
            frame_id: u32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes")),
            packet_idx: u16::from_le_bytes(bytes[4..6].try_into().expect("2 bytes")),
            data_shards: u16::from_le_bytes(bytes[6..8].try_into().expect("2 bytes")),
            parity_shards: bytes[8],
            flags: bytes[9],
            timestamp_us: u32::from_le_bytes(bytes[10..14].try_into().expect("4 bytes")),
        };
        Ok((hdr, &bytes[VIDEO_SUBHEADER_LEN..]))
    }
}

pub struct FramePacketizer;

impl FramePacketizer {
    /// Fragments `payload` into MTU-safe `VideoData` and `VideoFec` plaintexts ready for `seal_packet`.
    pub fn packetize(
        frame_id: u32,
        timestamp_us: u32,
        flags: u8,
        payload: &[u8],
        mtu: usize,
        fec_percent: u8,
    ) -> Result<Vec<(PacketKind, Vec<u8>)>> {
        if payload.is_empty() {
            return Ok(Vec::new());
        }
        let chunk_len = max_video_chunk_len(mtu);
        if chunk_len == 0 {
            bail!("MTU {mtu} is too small for video fragmentation");
        }

        let chunks: Vec<&[u8]> = payload.chunks(chunk_len).collect();
        if chunks.len() > MAX_DATA_SHARDS_PER_FRAME {
            bail!(
                "encoded frame ({} bytes, {} shards) exceeds max {MAX_DATA_SHARDS_PER_FRAME} shards",
                payload.len(),
                chunks.len()
            );
        }

        let data_shards_u16 = u16::try_from(chunks.len()).expect("bounded <= 4096");
        let parity_count = compute_parity_count(chunks.len(), fec_percent);
        let parity_shards_u8 = u8::try_from(parity_count).expect("bounded <= MAX_PARITY_SHARDS");
        let parity_bufs = encode_parity_shards(&chunks, parity_count)?;

        let mut packets = Vec::with_capacity(chunks.len() + parity_bufs.len());

        for (idx, chunk) in chunks.iter().enumerate() {
            let sub = VideoSubheader {
                frame_id,
                packet_idx: idx as u16,
                data_shards: data_shards_u16,
                parity_shards: parity_shards_u8,
                flags,
                timestamp_us,
            };
            let mut buf = Vec::with_capacity(VIDEO_SUBHEADER_LEN + chunk.len());
            buf.extend_from_slice(&sub.encode());
            buf.extend_from_slice(chunk);
            packets.push((PacketKind::VideoData, buf));
        }

        for (idx, parity) in parity_bufs.into_iter().enumerate() {
            let sub = VideoSubheader {
                frame_id,
                packet_idx: idx as u16,
                data_shards: data_shards_u16,
                parity_shards: parity_shards_u8,
                flags,
                timestamp_us,
            };
            let mut buf = Vec::with_capacity(VIDEO_SUBHEADER_LEN + parity.len());
            buf.extend_from_slice(&sub.encode());
            buf.extend_from_slice(&parity);
            packets.push((PacketKind::VideoFec, buf));
        }

        Ok(packets)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedFrame {
    pub frame_id: u32,
    pub timestamp_us: u32,
    pub flags: u8,
    pub data: Vec<u8>,
    pub fec_recovered: usize,
}

#[derive(Debug, Default)]
pub struct ReassembleOutcome {
    pub completed: Option<CompletedFrame>,
    pub request_keyframe: bool,
}

struct InFlightFrame {
    timestamp_us: u32,
    flags: u8,
    data_slots: Vec<Option<Vec<u8>>>,
    parity_slots: Vec<Option<Vec<u8>>>,
    received_data: usize,
    received_parity: usize,
    fec_recovered: usize,
}

impl InFlightFrame {
    fn new(sub: VideoSubheader) -> Self {
        Self {
            timestamp_us: sub.timestamp_us,
            flags: sub.flags,
            data_slots: vec![None; sub.data_shards as usize],
            parity_slots: vec![None; sub.parity_shards as usize],
            received_data: 0,
            received_parity: 0,
            fec_recovered: 0,
        }
    }

    fn is_complete(&self) -> bool {
        self.received_data == self.data_slots.len()
    }

    fn try_fec_recover(&mut self) {
        if self.is_complete() || self.parity_slots.is_empty() {
            return;
        }
        if self.received_data + self.received_parity < self.data_slots.len() {
            return;
        }
        let rec = recover_missing_shards(&mut self.data_slots, &self.parity_slots);
        if rec > 0 {
            self.received_data += rec;
            self.fec_recovered += rec;
        }
    }

    fn assemble(self, frame_id: u32) -> CompletedFrame {
        let total_len: usize = self
            .data_slots
            .iter()
            .map(|s| s.as_ref().map_or(0, Vec::len))
            .sum();
        let mut data = Vec::with_capacity(total_len);
        for shard in self.data_slots.into_iter().flatten() {
            data.extend_from_slice(&shard);
        }
        CompletedFrame {
            frame_id,
            timestamp_us: self.timestamp_us,
            flags: self.flags,
            data,
            fec_recovered: self.fec_recovered,
        }
    }
}

pub struct FrameReassembler {
    max_in_flight: usize,
    last_completed_frame: Option<u32>,
    in_flight: BTreeMap<u32, InFlightFrame>,
}

impl FrameReassembler {
    pub fn new(max_in_flight: usize) -> Self {
        Self {
            max_in_flight: max_in_flight.max(2),
            last_completed_frame: None,
            in_flight: BTreeMap::new(),
        }
    }

    fn is_not_newer(candidate: u32, reference: u32) -> bool {
        (candidate.wrapping_sub(reference) as i32) <= 0
    }

    fn is_strictly_older(candidate: u32, reference: u32) -> bool {
        (candidate.wrapping_sub(reference) as i32) < 0
    }

    pub fn push_packet(
        &mut self,
        kind: PacketKind,
        plaintext: &[u8],
    ) -> Result<ReassembleOutcome> {
        let (sub, body) = VideoSubheader::decode_split(plaintext)?;
        if sub.data_shards == 0 || (sub.data_shards as usize) > MAX_DATA_SHARDS_PER_FRAME {
            bail!("invalid data_shards count {}", sub.data_shards);
        }
        if (sub.parity_shards as usize) > MAX_PARITY_SHARDS {
            bail!("invalid parity_shards count {}", sub.parity_shards);
        }
        match kind {
            PacketKind::VideoData => {
                if sub.packet_idx >= sub.data_shards {
                    bail!(
                        "VideoData packet_idx {} >= data_shards {}",
                        sub.packet_idx,
                        sub.data_shards
                    );
                }
            }
            PacketKind::VideoFec => {
                if (sub.packet_idx as usize) >= (sub.parity_shards as usize) {
                    bail!(
                        "VideoFec packet_idx {} >= parity_shards {}",
                        sub.packet_idx,
                        sub.parity_shards
                    );
                }
            }
            other => bail!("non-video packet kind {other:?} passed to FrameReassembler"),
        }

        if let Some(last) = self.last_completed_frame
            && Self::is_not_newer(sub.frame_id, last)
        {
            return Ok(ReassembleOutcome::default());
        }

        let mut outcome = ReassembleOutcome::default();

        if !self.in_flight.contains_key(&sub.frame_id) && self.in_flight.len() >= self.max_in_flight
        {
            // Evict the oldest frame relative to wrap-around ordering.
            if let Some(&oldest_id) = self
                .in_flight
                .keys()
                .min_by(|&&a, &&b| {
                    let diff = a.wrapping_sub(b) as i32;
                    diff.cmp(&0)
                })
            {
                self.in_flight.remove(&oldest_id);
                outcome.request_keyframe = true;
            }
        }

        let entry = self
            .in_flight
            .entry(sub.frame_id)
            .or_insert_with(|| InFlightFrame::new(sub));

        // Guard against inconsistent metadata on the same frame_id.
        if entry.data_slots.len() != sub.data_shards as usize
            || entry.parity_slots.len() != sub.parity_shards as usize
        {
            bail!("inconsistent shard geometry for frame {}", sub.frame_id);
        }

        let idx = sub.packet_idx as usize;
        match kind {
            PacketKind::VideoData => {
                if entry.data_slots[idx].is_none() {
                    entry.data_slots[idx] = Some(body.to_vec());
                    entry.received_data += 1;
                }
            }
            PacketKind::VideoFec => {
                if entry.parity_slots[idx].is_none() {
                    entry.parity_slots[idx] = Some(body.to_vec());
                    entry.received_parity += 1;
                }
            }
            _ => unreachable!(),
        }

        entry.try_fec_recover();

        if entry.is_complete() {
            let completed = self
                .in_flight
                .remove(&sub.frame_id)
                .expect("just checked")
                .assemble(sub.frame_id);

            let older_ids: Vec<u32> = self
                .in_flight
                .keys()
                .copied()
                .filter(|&id| Self::is_strictly_older(id, sub.frame_id))
                .collect();
            if !older_ids.is_empty() {
                for id in older_ids {
                    self.in_flight.remove(&id);
                }
                outcome.request_keyframe = true;
            }

            self.last_completed_frame = Some(sub.frame_id);
            outcome.completed = Some(completed);
        }

        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{Role, SessionCrypto};
    use crate::packet::{DEFAULT_MTU, open_packet, seal_packet};

    #[test]
    fn mtu_budget_holds_across_frame_sizes() {
        let key = [11u8; 32];
        let mut host = SessionCrypto::new(&key, Role::Host);
        let mut client = SessionCrypto::new(&key, Role::Client);

        for size in [1usize, 17, 1156, 1157, 15_000, 128_000] {
            let payload: Vec<u8> = (0..size).map(|i| ((i * 7) & 0xff) as u8).collect();
            let pkts =
                FramePacketizer::packetize(size as u32, 1234, FRAME_FLAG_KEYFRAME, &payload, DEFAULT_MTU, 15)
                    .unwrap();
            assert!(!pkts.is_empty());

            let mut reassembler = FrameReassembler::new(8);
            let mut completed = None;
            for (kind, pt) in pkts {
                let dgram = seal_packet(&mut host, kind, &pt, DEFAULT_MTU).unwrap();
                assert!(
                    dgram.len() <= DEFAULT_MTU,
                    "size={size} produced datagram {} > {DEFAULT_MTU}",
                    dgram.len()
                );
                let (k, _, dec) = open_packet(&mut client, &dgram).unwrap();
                let res = reassembler.push_packet(k, &dec).unwrap();
                if res.completed.is_some() {
                    completed = res.completed;
                }
            }
            let frame = completed.expect("frame should complete");
            assert_eq!(frame.data, payload);
            assert_eq!(frame.flags, FRAME_FLAG_KEYFRAME);
        }
    }

    #[test]
    fn fec_recovers_dropped_packet_without_retransmit() {
        let payload: Vec<u8> = (0..10_000).map(|i| (i % 251) as u8).collect();
        let pkts =
            FramePacketizer::packetize(1, 5000, 0, &payload, DEFAULT_MTU, 20).unwrap();
        let mut reassembler = FrameReassembler::new(8);
        let mut completed = None;

        // Intentionally drop data packet #3
        for (i, (kind, pt)) in pkts.into_iter().enumerate() {
            if i == 3 {
                continue;
            }
            let res = reassembler.push_packet(kind, &pt).unwrap();
            if res.completed.is_some() {
                completed = res.completed;
            }
        }
        let frame = completed.expect("FEC should recover missing shard #3");
        assert_eq!(frame.data, payload);
        assert_eq!(frame.fec_recovered, 1);
    }

    #[test]
    fn unrecoverable_loss_triggers_keyframe_request() {
        let payload = vec![5u8; 8000];
        let p1 = FramePacketizer::packetize(1, 1000, 0, &payload, DEFAULT_MTU, 0).unwrap();
        let p2 = FramePacketizer::packetize(2, 2000, 0, &payload, DEFAULT_MTU, 0).unwrap();

        let mut reassembler = FrameReassembler::new(8);
        // Push partial frame 1 (drop packet 0)
        for (i, (kind, pt)) in p1.into_iter().enumerate() {
            if i == 0 {
                continue;
            }
            let res = reassembler.push_packet(kind, &pt).unwrap();
            assert!(res.completed.is_none());
        }

        // Complete frame 2 -> evicts incomplete frame 1 and requests keyframe
        let mut saw_pli = false;
        let mut got_f2 = false;
        for (kind, pt) in p2 {
            let res = reassembler.push_packet(kind, &pt).unwrap();
            saw_pli |= res.request_keyframe;
            if res.completed.is_some() {
                got_f2 = true;
            }
        }
        assert!(got_f2);
        assert!(saw_pli);
    }
}
