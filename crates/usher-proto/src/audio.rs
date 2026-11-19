use anyhow::{Result, bail};

pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u8 = 2;
pub const AUDIO_FRAME_SAMPLES_5MS: usize = 240;
pub const AUDIO_HEADER_LEN: usize = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioCodec {
    Opus = 1,
    Pcm16 = 2,
}

impl AudioCodec {
    pub fn from_u8(val: u8) -> Result<Self> {
        match val {
            1 => Ok(Self::Opus),
            2 => Ok(Self::Pcm16),
            other => bail!("unknown audio codec {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub seq: u32,
    pub timestamp_us: u32,
    pub codec: AudioCodec,
    pub samples_per_channel: u16,
    pub payload: Vec<u8>,
}

impl AudioPacket {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(AUDIO_HEADER_LEN + self.payload.len());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.timestamp_us.to_le_bytes());
        out.push(self.codec as u8);
        out.extend_from_slice(&self.samples_per_channel.to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < AUDIO_HEADER_LEN {
            bail!(
                "audio packet too short: {} < {AUDIO_HEADER_LEN}",
                bytes.len()
            );
        }
        Ok(Self {
            seq: u32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes")),
            timestamp_us: u32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes")),
            codec: AudioCodec::from_u8(bytes[8])?,
            samples_per_channel: u16::from_le_bytes(bytes[9..11].try_into().expect("2 bytes")),
            payload: bytes[AUDIO_HEADER_LEN..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_packet_roundtrip() {
        let pkt = AudioPacket {
            seq: 42,
            timestamp_us: 90_000,
            codec: AudioCodec::Opus,
            samples_per_channel: AUDIO_FRAME_SAMPLES_5MS as u16,
            payload: vec![1, 2, 3, 4, 5],
        };
        let enc = pkt.encode();
        let dec = AudioPacket::decode(&enc).unwrap();
        assert_eq!(dec, pkt);
    }
}
