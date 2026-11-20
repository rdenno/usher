use crate::crypto::SessionCrypto;
use crate::handshake::{QualityPreference, VideoCodec};
use anyhow::{Result, bail};

pub const MAGIC: [u8; 2] = *b"US";
pub const OUTER_HEADER_LEN: usize = 12;
pub const AEAD_TAG_LEN: usize = 16;
pub const DEFAULT_MTU: usize = 1200;
const WIRE_VERSION_NIBBLE: u8 = 1;
pub const STREAM_STATUS_FLAG_STEPPED_DOWN: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketKind {
    VideoData = 0,
    VideoFec = 1,
    Audio = 2,
    Input = 3,
    Control = 4,
    Ping = 5,
    Pong = 6,
    Clipboard = 7,
}

impl PacketKind {
    pub fn from_u8(val: u8) -> Result<Self> {
        match val {
            0 => Ok(Self::VideoData),
            1 => Ok(Self::VideoFec),
            2 => Ok(Self::Audio),
            3 => Ok(Self::Input),
            4 => Ok(Self::Control),
            5 => Ok(Self::Ping),
            6 => Ok(Self::Pong),
            7 => Ok(Self::Clipboard),
            other => bail!("unknown packet kind: {other}"),
        }
    }
}

pub const fn max_plaintext_len(mtu: usize) -> usize {
    mtu.saturating_sub(OUTER_HEADER_LEN + AEAD_TAG_LEN)
}

/// Encrypts `plaintext` into a single authenticated UDP datagram bounded by `mtu`.
pub fn seal_packet(
    crypto: &mut SessionCrypto,
    kind: PacketKind,
    plaintext: &[u8],
    mtu: usize,
) -> Result<Vec<u8>> {
    let total_len = OUTER_HEADER_LEN + plaintext.len() + AEAD_TAG_LEN;
    if total_len > mtu {
        bail!("packet size {total_len} exceeds MTU {mtu}");
    }

    let (header, ct) = crypto.encrypt_with_seq(
        |seq| {
            let mut hdr = [0u8; OUTER_HEADER_LEN];
            hdr[0..2].copy_from_slice(&MAGIC);
            hdr[2] = (WIRE_VERSION_NIBBLE << 4) | (kind as u8);
            hdr[3] = 0;
            hdr[4..12].copy_from_slice(&seq.to_le_bytes());
            hdr
        },
        plaintext,
    )?;

    let mut datagram = Vec::with_capacity(OUTER_HEADER_LEN + ct.len());
    datagram.extend_from_slice(&header);
    datagram.extend_from_slice(&ct);
    Ok(datagram)
}

/// Validates envelope framing, authenticates, anti-replays, and decrypts an incoming datagram.
pub fn open_packet(
    crypto: &mut SessionCrypto,
    datagram: &[u8],
) -> Result<(PacketKind, u64, Vec<u8>)> {
    if datagram.len() < OUTER_HEADER_LEN + AEAD_TAG_LEN {
        bail!("datagram too short ({} bytes)", datagram.len());
    }
    if datagram[0..2] != MAGIC {
        bail!("invalid packet magic");
    }
    let ver = datagram[2] >> 4;
    if ver != WIRE_VERSION_NIBBLE {
        bail!("unsupported wire version {ver}");
    }
    let kind = PacketKind::from_u8(datagram[2] & 0x0f)?;
    let seq = u64::from_le_bytes(datagram[4..12].try_into().expect("8 bytes"));
    let aad = &datagram[..OUTER_HEADER_LEN];
    let ciphertext = &datagram[OUTER_HEADER_LEN..];

    let plaintext = crypto.decrypt(seq, aad, ciphertext)?;
    Ok((kind, seq, plaintext))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlMessage {
    ClientHello {
        client_version: u16,
        capabilities: u32,
    },
    RequestKeyframe,
    Resize {
        width: u16,
        height: u16,
    },
    SetPointerMode {
        relative: bool,
    },
    Disconnect,
    NetworkFeedback {
        loss_after_fec_bps: u16,
        fec_recovery_bps: u16,
        rtt_us: u32,
        late_frames: u16,
        recv_kbps: u32,
    },
    Reconfigure {
        max_bitrate_kbps: u32,
        width: u16,
        height: u16,
        fps: u16,
        codec: u8,
        prefer: u8,
        audio: u8,
        display: u8,
    },
    StreamStatus {
        width: u16,
        height: u16,
        fps: u16,
        codec: VideoCodec,
        target_bitrate_kbps: u32,
        max_bitrate_kbps: u32,
        fec_percent: u8,
        display_index: u8,
        prefer: QualityPreference,
        audio_enabled: bool,
        flags: u8,
    },
}

impl ControlMessage {
    pub const fn reconfigure_unchanged() -> Self {
        Self::Reconfigure {
            max_bitrate_kbps: 0,
            width: 0,
            height: 0,
            fps: 0,
            codec: 0,
            prefer: 0,
            audio: 0,
            display: u8::MAX,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        match *self {
            ControlMessage::ClientHello {
                client_version,
                capabilities,
            } => {
                let mut out = Vec::with_capacity(7);
                out.push(0x01);
                out.extend_from_slice(&client_version.to_le_bytes());
                out.extend_from_slice(&capabilities.to_le_bytes());
                out
            }
            ControlMessage::RequestKeyframe => vec![0x02],
            ControlMessage::Resize { width, height } => {
                let mut out = Vec::with_capacity(5);
                out.push(0x03);
                out.extend_from_slice(&width.to_le_bytes());
                out.extend_from_slice(&height.to_le_bytes());
                out
            }
            ControlMessage::SetPointerMode { relative } => {
                vec![0x04, u8::from(relative)]
            }
            ControlMessage::Disconnect => vec![0x05],
            ControlMessage::NetworkFeedback {
                loss_after_fec_bps,
                fec_recovery_bps,
                rtt_us,
                late_frames,
                recv_kbps,
            } => {
                let mut out = Vec::with_capacity(15);
                out.push(0x06);
                out.extend_from_slice(&loss_after_fec_bps.to_le_bytes());
                out.extend_from_slice(&fec_recovery_bps.to_le_bytes());
                out.extend_from_slice(&rtt_us.to_le_bytes());
                out.extend_from_slice(&late_frames.to_le_bytes());
                out.extend_from_slice(&recv_kbps.to_le_bytes());
                out
            }
            ControlMessage::Reconfigure {
                max_bitrate_kbps,
                width,
                height,
                fps,
                codec,
                prefer,
                audio,
                display,
            } => {
                let mut out = Vec::with_capacity(15);
                out.push(0x07);
                out.extend_from_slice(&max_bitrate_kbps.to_le_bytes());
                out.extend_from_slice(&width.to_le_bytes());
                out.extend_from_slice(&height.to_le_bytes());
                out.extend_from_slice(&fps.to_le_bytes());
                out.push(codec);
                out.push(prefer);
                out.push(audio);
                out.push(display);
                out
            }
            ControlMessage::StreamStatus {
                width,
                height,
                fps,
                codec,
                target_bitrate_kbps,
                max_bitrate_kbps,
                fec_percent,
                display_index,
                prefer,
                audio_enabled,
                flags,
            } => {
                let mut out = Vec::with_capacity(21);
                out.push(0x08);
                out.extend_from_slice(&width.to_le_bytes());
                out.extend_from_slice(&height.to_le_bytes());
                out.extend_from_slice(&fps.to_le_bytes());
                out.push(codec.to_u8());
                out.extend_from_slice(&target_bitrate_kbps.to_le_bytes());
                out.extend_from_slice(&max_bitrate_kbps.to_le_bytes());
                out.push(fec_percent);
                out.push(display_index);
                out.push(prefer.to_u8());
                out.push(u8::from(audio_enabled));
                out.push(flags);
                out
            }
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (&tag, rest) = bytes
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("empty control message"))?;
        match tag {
            0x01 => {
                if rest.len() != 6 {
                    bail!("invalid ClientHello length {}", rest.len());
                }
                Ok(Self::ClientHello {
                    client_version: u16::from_le_bytes([rest[0], rest[1]]),
                    capabilities: u32::from_le_bytes([rest[2], rest[3], rest[4], rest[5]]),
                })
            }
            0x02 => Ok(Self::RequestKeyframe),
            0x03 => {
                if rest.len() != 4 {
                    bail!("invalid Resize length {}", rest.len());
                }
                Ok(Self::Resize {
                    width: u16::from_le_bytes([rest[0], rest[1]]),
                    height: u16::from_le_bytes([rest[2], rest[3]]),
                })
            }
            0x04 => {
                if rest.len() != 1 {
                    bail!("invalid SetPointerMode length {}", rest.len());
                }
                Ok(Self::SetPointerMode {
                    relative: rest[0] != 0,
                })
            }
            0x05 => Ok(Self::Disconnect),
            0x06 => {
                if rest.len() != 14 {
                    bail!("invalid NetworkFeedback length {}", rest.len());
                }
                Ok(Self::NetworkFeedback {
                    loss_after_fec_bps: u16::from_le_bytes([rest[0], rest[1]]),
                    fec_recovery_bps: u16::from_le_bytes([rest[2], rest[3]]),
                    rtt_us: u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]),
                    late_frames: u16::from_le_bytes([rest[8], rest[9]]),
                    recv_kbps: u32::from_le_bytes([rest[10], rest[11], rest[12], rest[13]]),
                })
            }
            0x07 => {
                if rest.len() != 14 {
                    bail!("invalid Reconfigure length {}", rest.len());
                }
                Ok(Self::Reconfigure {
                    max_bitrate_kbps: u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]),
                    width: u16::from_le_bytes([rest[4], rest[5]]),
                    height: u16::from_le_bytes([rest[6], rest[7]]),
                    fps: u16::from_le_bytes([rest[8], rest[9]]),
                    codec: rest[10],
                    prefer: rest[11],
                    audio: rest[12],
                    display: rest[13],
                })
            }
            0x08 => {
                if rest.len() != 20 {
                    bail!("invalid StreamStatus length {}", rest.len());
                }
                Ok(Self::StreamStatus {
                    width: u16::from_le_bytes([rest[0], rest[1]]),
                    height: u16::from_le_bytes([rest[2], rest[3]]),
                    fps: u16::from_le_bytes([rest[4], rest[5]]),
                    codec: VideoCodec::from_u8(rest[6])?,
                    target_bitrate_kbps: u32::from_le_bytes([rest[7], rest[8], rest[9], rest[10]]),
                    max_bitrate_kbps: u32::from_le_bytes([rest[11], rest[12], rest[13], rest[14]]),
                    fec_percent: rest[15],
                    display_index: rest[16],
                    prefer: QualityPreference::from_u8(rest[17])?,
                    audio_enabled: rest[18] != 0,
                    flags: rest[19],
                })
            }
            other => bail!("unknown control message tag {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PingPayload {
    pub sent_us: u64,
}

impl PingPayload {
    pub fn encode(&self) -> [u8; 8] {
        self.sent_us.to_le_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 8 {
            bail!("invalid PingPayload length {}", bytes.len());
        }
        Ok(Self {
            sent_us: u64::from_le_bytes(bytes.try_into().expect("8 bytes")),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PongPayload {
    pub sent_us: u64,
    pub host_recv_us: u64,
}

impl PongPayload {
    pub fn encode(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[0..8].copy_from_slice(&self.sent_us.to_le_bytes());
        out[8..16].copy_from_slice(&self.host_recv_us.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 16 {
            bail!("invalid PongPayload length {}", bytes.len());
        }
        Ok(Self {
            sent_us: u64::from_le_bytes(bytes[0..8].try_into().expect("8 bytes")),
            host_recv_us: u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{Role, SessionCrypto};

    #[test]
    fn seal_open_and_tampered_aad_rejected() {
        let key = [9u8; 32];
        let mut client = SessionCrypto::new(&key, Role::Client);
        let mut host = SessionCrypto::new(&key, Role::Host);

        let ctrl = ControlMessage::ClientHello {
            client_version: 1,
            capabilities: 0x07,
        };
        let mut dgram = seal_packet(&mut client, PacketKind::Control, &ctrl.encode(), DEFAULT_MTU)
            .unwrap();
        assert!(dgram.len() <= DEFAULT_MTU);

        // Tamper reserved byte in outer AAD header
        dgram[3] = 0x55;
        assert!(open_packet(&mut host, &dgram).is_err());

        // Restore byte and verify
        dgram[3] = 0;
        let (kind, seq, pt) = open_packet(&mut host, &dgram).unwrap();
        assert_eq!(kind, PacketKind::Control);
        assert_eq!(seq, 0);
        assert_eq!(ControlMessage::decode(&pt).unwrap(), ctrl);
    }

    #[test]
    fn mtu_overflow_is_rejected() {
        let mut host = SessionCrypto::new(&[3u8; 32], Role::Host);
        let too_big = vec![0u8; max_plaintext_len(DEFAULT_MTU) + 1];
        assert!(seal_packet(&mut host, PacketKind::VideoData, &too_big, DEFAULT_MTU).is_err());
    }

    #[test]
    fn roundtrip_feedback_reconfigure_and_stream_status() {
        let msgs = [
            ControlMessage::NetworkFeedback {
                loss_after_fec_bps: 250,
                fec_recovery_bps: 120,
                rtt_us: 14_200,
                late_frames: 2,
                recv_kbps: 24_500,
            },
            ControlMessage::Reconfigure {
                max_bitrate_kbps: 45_000,
                width: 1920,
                height: 1080,
                fps: 120,
                codec: 3,
                prefer: 2,
                audio: 1,
                display: 1,
            },
            ControlMessage::StreamStatus {
                width: 1920,
                height: 1080,
                fps: 120,
                codec: VideoCodec::Hevc,
                target_bitrate_kbps: 32_000,
                max_bitrate_kbps: 45_000,
                fec_percent: 15,
                display_index: 1,
                prefer: QualityPreference::Quality,
                audio_enabled: false,
                flags: STREAM_STATUS_FLAG_STEPPED_DOWN,
            },
        ];
        for msg in msgs {
            let encoded = msg.encode();
            let decoded = ControlMessage::decode(&encoded).unwrap();
            assert_eq!(decoded, msg);
        }
    }
}
