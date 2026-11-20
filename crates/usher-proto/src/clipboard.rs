use anyhow::{Result, bail};

pub const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;
pub const CLIPBOARD_HDR_LEN: usize = 12;

/// Splits UTF-8 clipboard text into MTU-sized wire chunks:
/// `[transfer_id: u32 LE][chunk_idx: u16 LE][chunk_count: u16 LE][total_len: u32 LE][utf8_slice]`.
pub fn packetize_clipboard_text(transfer_id: u32, text: &str, max_plaintext: usize) -> Vec<Vec<u8>> {
    if text.is_empty() || max_plaintext <= CLIPBOARD_HDR_LEN {
        return Vec::new();
    }

    let capped = if text.len() > MAX_CLIPBOARD_BYTES {
        let mut end = MAX_CLIPBOARD_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        &text.as_bytes()[..end]
    } else {
        text.as_bytes()
    };

    if capped.is_empty() {
        return Vec::new();
    }

    let payload_cap = max_plaintext - CLIPBOARD_HDR_LEN;
    let total_len = capped.len() as u32;
    let chunk_count_usize = capped.len().div_ceil(payload_cap);
    let Ok(chunk_count) = u16::try_from(chunk_count_usize) else {
        return Vec::new();
    };

    capped
        .chunks(payload_cap)
        .enumerate()
        .map(|(idx, slice)| {
            let mut out = Vec::with_capacity(CLIPBOARD_HDR_LEN + slice.len());
            out.extend_from_slice(&transfer_id.to_le_bytes());
            out.extend_from_slice(&(idx as u16).to_le_bytes());
            out.extend_from_slice(&chunk_count.to_le_bytes());
            out.extend_from_slice(&total_len.to_le_bytes());
            out.extend_from_slice(slice);
            out
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct ClipboardReassembler {
    active_id: Option<u32>,
    total_len: usize,
    chunk_count: u16,
    chunks: Vec<Option<Vec<u8>>>,
    received: u16,
    last_completed_id: Option<u32>,
}

impl ClipboardReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_chunk(&mut self, bytes: &[u8]) -> Result<Option<String>> {
        if bytes.len() < CLIPBOARD_HDR_LEN {
            bail!("short clipboard chunk ({} bytes)", bytes.len());
        }

        let transfer_id = u32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes"));
        let chunk_idx = u16::from_le_bytes([bytes[4], bytes[5]]);
        let chunk_count = u16::from_le_bytes([bytes[6], bytes[7]]);
        let total_len = u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes")) as usize;
        let payload = &bytes[CLIPBOARD_HDR_LEN..];

        if total_len == 0 || total_len > MAX_CLIPBOARD_BYTES {
            bail!("invalid clipboard total_len {total_len}");
        }
        if chunk_count == 0 || chunk_idx >= chunk_count {
            bail!("invalid clipboard chunk {chunk_idx}/{chunk_count}");
        }

        if self.last_completed_id == Some(transfer_id) {
            return Ok(None);
        }

        if self.active_id != Some(transfer_id) {
            self.active_id = Some(transfer_id);
            self.total_len = total_len;
            self.chunk_count = chunk_count;
            self.chunks = vec![None; chunk_count as usize];
            self.received = 0;
        } else if self.total_len != total_len || self.chunk_count != chunk_count {
            bail!("clipboard transfer {transfer_id} metadata mismatch");
        }

        let slot = &mut self.chunks[chunk_idx as usize];
        if slot.is_none() {
            *slot = Some(payload.to_vec());
            self.received += 1;
        }

        if self.received < self.chunk_count {
            return Ok(None);
        }

        let mut assembled = Vec::with_capacity(self.total_len);
        for p in self.chunks.drain(..).flatten() {
            assembled.extend_from_slice(&p);
        }
        self.active_id = None;
        self.received = 0;
        self.last_completed_id = Some(transfer_id);

        if assembled.len() != self.total_len {
            bail!(
                "clipboard assembled {}B != expected {}B",
                assembled.len(),
                self.total_len
            );
        }

        let text = String::from_utf8(assembled)?;
        Ok(Some(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_single_chunk() {
        let chunks = packetize_clipboard_text(1, "hello usher clipboard", 1024);
        assert_eq!(chunks.len(), 1);
        let mut r = ClipboardReassembler::new();
        assert_eq!(
            r.push_chunk(&chunks[0]).unwrap(),
            Some("hello usher clipboard".to_string())
        );
        // Duplicate burst packet is ignored once transfer completes.
        assert_eq!(r.push_chunk(&chunks[0]).unwrap(), None);
    }

    #[test]
    fn out_of_order_multi_chunk_and_dedup() {
        let text = "0123456789abcdef".repeat(200);
        let chunks = packetize_clipboard_text(42, &text, 120);
        assert!(chunks.len() > 2);

        let mut r = ClipboardReassembler::new();
        for chunk in chunks[1..].iter().rev() {
            assert_eq!(r.push_chunk(chunk).unwrap(), None);
        }
        // Duplicate middle chunk doesn't prematurely complete.
        assert_eq!(r.push_chunk(&chunks[1]).unwrap(), None);
        // Final missing chunk completes transfer.
        assert_eq!(r.push_chunk(&chunks[0]).unwrap(), Some(text));
        // Repeated packets after completion are ignored.
        assert_eq!(r.push_chunk(&chunks[0]).unwrap(), None);
    }

    #[test]
    fn rejects_oversize_or_corrupt_chunks() {
        let mut r = ClipboardReassembler::new();
        assert!(r.push_chunk(&[0u8; 4]).is_err());

        let mut bad = vec![0u8; CLIPBOARD_HDR_LEN + 4];
        bad[6..8].copy_from_slice(&1u16.to_le_bytes());
        bad[8..12].copy_from_slice(&((MAX_CLIPBOARD_BYTES as u32) + 1).to_le_bytes());
        assert!(r.push_chunk(&bad).is_err());

        let huge = "a".repeat(MAX_CLIPBOARD_BYTES + 500);
        let truncated = packetize_clipboard_text(7, &huge, 1024);
        let total: usize = truncated.iter().map(|c| c.len() - CLIPBOARD_HDR_LEN).sum();
        assert_eq!(total, MAX_CLIPBOARD_BYTES);
    }
}
