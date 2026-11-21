use anyhow::{Result, bail};

pub const MAX_CURSOR_DIM: u16 = 256;
pub const MAX_CURSOR_RGBA_BYTES: usize = (MAX_CURSOR_DIM as usize) * (MAX_CURSOR_DIM as usize) * 4;
pub const CURSOR_POS_LEN: usize = 10;
pub const CURSOR_SHAPE_HDR_LEN: usize = 17;

const TAG_POSITION: u8 = 0;
const TAG_SHAPE_CHUNK: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorPacket {
    Position {
        seq: u32,
        visible: bool,
        x_norm: u16,
        y_norm: u16,
    },
    ShapeChunk {
        shape_id: u32,
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
        chunk_idx: u16,
        chunk_count: u16,
        data: Vec<u8>,
    },
}

impl CursorPacket {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Position {
                seq,
                visible,
                x_norm,
                y_norm,
            } => {
                let mut out = Vec::with_capacity(CURSOR_POS_LEN);
                out.push(TAG_POSITION);
                out.extend_from_slice(&seq.to_le_bytes());
                out.push(u8::from(*visible));
                out.extend_from_slice(&x_norm.to_le_bytes());
                out.extend_from_slice(&y_norm.to_le_bytes());
                out
            }
            Self::ShapeChunk {
                shape_id,
                width,
                height,
                hotspot_x,
                hotspot_y,
                chunk_idx,
                chunk_count,
                data,
            } => {
                let mut out = Vec::with_capacity(CURSOR_SHAPE_HDR_LEN + data.len());
                out.push(TAG_SHAPE_CHUNK);
                out.extend_from_slice(&shape_id.to_le_bytes());
                out.extend_from_slice(&width.to_le_bytes());
                out.extend_from_slice(&height.to_le_bytes());
                out.extend_from_slice(&hotspot_x.to_le_bytes());
                out.extend_from_slice(&hotspot_y.to_le_bytes());
                out.extend_from_slice(&chunk_idx.to_le_bytes());
                out.extend_from_slice(&chunk_count.to_le_bytes());
                out.extend_from_slice(data);
                out
            }
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let Some((&tag, rest)) = bytes.split_first() else {
            bail!("empty cursor packet");
        };
        match tag {
            TAG_POSITION => {
                if bytes.len() < CURSOR_POS_LEN {
                    bail!("short cursor position packet ({} bytes)", bytes.len());
                }
                let seq = u32::from_le_bytes(rest[0..4].try_into().expect("4 bytes"));
                let visible = (rest[4] & 1) != 0;
                let x_norm = u16::from_le_bytes([rest[5], rest[6]]);
                let y_norm = u16::from_le_bytes([rest[7], rest[8]]);
                Ok(Self::Position {
                    seq,
                    visible,
                    x_norm,
                    y_norm,
                })
            }
            TAG_SHAPE_CHUNK => {
                if bytes.len() < CURSOR_SHAPE_HDR_LEN {
                    bail!("short cursor shape packet ({} bytes)", bytes.len());
                }
                let shape_id = u32::from_le_bytes(rest[0..4].try_into().expect("4 bytes"));
                let width = u16::from_le_bytes([rest[4], rest[5]]);
                let height = u16::from_le_bytes([rest[6], rest[7]]);
                let hotspot_x = u16::from_le_bytes([rest[8], rest[9]]);
                let hotspot_y = u16::from_le_bytes([rest[10], rest[11]]);
                let chunk_idx = u16::from_le_bytes([rest[12], rest[13]]);
                let chunk_count = u16::from_le_bytes([rest[14], rest[15]]);
                let data = rest[16..].to_vec();
                Ok(Self::ShapeChunk {
                    shape_id,
                    width,
                    height,
                    hotspot_x,
                    hotspot_y,
                    chunk_idx,
                    chunk_count,
                    data,
                })
            }
            other => bail!("unknown cursor packet tag: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub shape_id: u32,
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub rgba: Vec<u8>,
}

/// Splits an RGBA8 cursor bitmap into MTU-sized wire packets.
pub fn packetize_cursor_shape(
    shape_id: u32,
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
    rgba: &[u8],
    max_plaintext: usize,
) -> Vec<Vec<u8>> {
    if width == 0
        || height == 0
        || width > MAX_CURSOR_DIM
        || height > MAX_CURSOR_DIM
        || max_plaintext <= CURSOR_SHAPE_HDR_LEN
    {
        return Vec::new();
    }
    let expected_len = (width as usize) * (height as usize) * 4;
    if rgba.len() != expected_len || expected_len > MAX_CURSOR_RGBA_BYTES {
        return Vec::new();
    }

    let payload_cap = max_plaintext - CURSOR_SHAPE_HDR_LEN;
    let chunk_count_usize = rgba.len().div_ceil(payload_cap);
    let Ok(chunk_count) = u16::try_from(chunk_count_usize) else {
        return Vec::new();
    };

    rgba.chunks(payload_cap)
        .enumerate()
        .map(|(idx, slice)| {
            CursorPacket::ShapeChunk {
                shape_id,
                width,
                height,
                hotspot_x,
                hotspot_y,
                chunk_idx: idx as u16,
                chunk_count,
                data: slice.to_vec(),
            }
            .encode()
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct CursorShapeReassembler {
    active_id: Option<u32>,
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
    chunk_count: u16,
    chunks: Vec<Option<Vec<u8>>>,
    received: u16,
    last_completed_id: Option<u32>,
}

impl CursorShapeReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_chunk(&mut self, bytes: &[u8]) -> Result<Option<CursorShape>> {
        let pkt = CursorPacket::decode(bytes)?;
        self.push_packet(pkt)
    }

    pub fn push_packet(&mut self, pkt: CursorPacket) -> Result<Option<CursorShape>> {
        let CursorPacket::ShapeChunk {
            shape_id,
            width,
            height,
            hotspot_x,
            hotspot_y,
            chunk_idx,
            chunk_count,
            data,
        } = pkt
        else {
            bail!("expected cursor shape chunk packet");
        };

        if width == 0 || height == 0 || width > MAX_CURSOR_DIM || height > MAX_CURSOR_DIM {
            bail!("invalid cursor shape dimensions {width}x{height}");
        }
        let expected_len = (width as usize) * (height as usize) * 4;
        if expected_len > MAX_CURSOR_RGBA_BYTES {
            bail!("cursor shape too large ({expected_len} bytes)");
        }
        if chunk_count == 0 || chunk_idx >= chunk_count {
            bail!("invalid cursor chunk {chunk_idx}/{chunk_count}");
        }
        if data.is_empty() || data.len() > expected_len {
            bail!("invalid cursor chunk length {}", data.len());
        }

        if self.last_completed_id == Some(shape_id) {
            return Ok(None);
        }

        if self.active_id != Some(shape_id) {
            self.active_id = Some(shape_id);
            self.width = width;
            self.height = height;
            self.hotspot_x = hotspot_x;
            self.hotspot_y = hotspot_y;
            self.chunk_count = chunk_count;
            self.chunks = vec![None; chunk_count as usize];
            self.received = 0;
        } else if self.width != width
            || self.height != height
            || self.hotspot_x != hotspot_x
            || self.hotspot_y != hotspot_y
            || self.chunk_count != chunk_count
        {
            self.active_id = None;
            self.chunks.clear();
            self.received = 0;
            bail!("cursor shape {shape_id} metadata mismatch");
        }

        let slot = &mut self.chunks[chunk_idx as usize];
        if slot.is_none() {
            *slot = Some(data);
            self.received += 1;
        }

        if self.received < self.chunk_count {
            return Ok(None);
        }

        let mut rgba = Vec::with_capacity(expected_len);
        for bytes in self.chunks.drain(..).flatten() {
            rgba.extend_from_slice(&bytes);
        }
        self.active_id = None;
        self.received = 0;
        if rgba.len() != expected_len {
            bail!(
                "cursor shape {shape_id} assembled length {} != expected {expected_len}",
                rgba.len()
            );
        }

        self.last_completed_id = Some(shape_id);
        Ok(Some(CursorShape {
            shape_id,
            width,
            height,
            hotspot_x,
            hotspot_y,
            rgba,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_position_roundtrip() {
        let pkt = CursorPacket::Position {
            seq: 42,
            visible: true,
            x_norm: 32768,
            y_norm: 65535,
        };
        let encoded = pkt.encode();
        assert_eq!(encoded.len(), CURSOR_POS_LEN);
        assert_eq!(CursorPacket::decode(&encoded).unwrap(), pkt);
    }

    #[test]
    fn cursor_shape_packetize_and_reassemble_out_of_order() {
        let rgba: Vec<u8> = (0..(32 * 32 * 4)).map(|i| (i % 251) as u8).collect();
        let chunks = packetize_cursor_shape(99, 32, 32, 4, 8, &rgba, 512);
        assert!(chunks.len() > 2);

        let mut reasm = CursorShapeReassembler::new();
        for chunk in chunks.iter().rev() {
            let decoded = CursorPacket::decode(chunk).unwrap();
            let res = reasm.push_packet(decoded).unwrap();
            if let Some(shape) = res {
                assert_eq!(shape.shape_id, 99);
                assert_eq!(shape.width, 32);
                assert_eq!(shape.height, 32);
                assert_eq!(shape.hotspot_x, 4);
                assert_eq!(shape.hotspot_y, 8);
                assert_eq!(shape.rgba, rgba);
                // Duplicate chunk for same shape_id should be ignored.
                assert!(reasm.push_chunk(&chunks[0]).unwrap().is_none());
                return;
            }
        }
        panic!("cursor shape never completed");
    }

    #[test]
    fn cursor_shape_rejects_oversize_or_mismatched_chunks() {
        assert!(packetize_cursor_shape(1, 0, 16, 0, 0, &[], 512).is_empty());
        assert!(packetize_cursor_shape(1, 257, 1, 0, 0, &[0u8; 257 * 4], 512).is_empty());
        let mut reasm = CursorShapeReassembler::new();
        let bad = CursorPacket::ShapeChunk {
            shape_id: 7,
            width: 300,
            height: 4,
            hotspot_x: 0,
            hotspot_y: 0,
            chunk_idx: 0,
            chunk_count: 1,
            data: vec![1, 2, 3, 4],
        };
        assert!(reasm.push_packet(bad).is_err());
    }
}
