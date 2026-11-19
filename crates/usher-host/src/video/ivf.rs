use anyhow::{Result, bail};

#[derive(Default)]
pub struct IvfReader {
    header_done: bool,
    buf: Vec<u8>,
}

impl IvfReader {
    pub fn push_bytes(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        if !self.header_done {
            if self.buf.len() < 32 {
                return Ok(out);
            }
            if &self.buf[0..4] != b"DKIF" {
                bail!("invalid IVF magic from AV1 encoder");
            }
            self.buf.drain(..32);
            self.header_done = true;
        }

        while self.buf.len() >= 12 {
            let frame_len = u32::from_le_bytes(self.buf[0..4].try_into().expect("4")) as usize;
            if self.buf.len() < 12 + frame_len {
                break;
            }
            let frame = self.buf[12..12 + frame_len].to_vec();
            self.buf.drain(..12 + frame_len);
            out.push(frame);
        }
        Ok(out)
    }
}
