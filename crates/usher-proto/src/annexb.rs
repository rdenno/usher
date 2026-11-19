use crate::handshake::VideoCodec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

#[derive(Default)]
struct AuBuilder {
    current_au: Vec<u8>,
    current_has_vcl: bool,
    current_is_keyframe: bool,
}

impl AuBuilder {
    fn take_current_au(&mut self) -> Option<AccessUnit> {
        if self.current_au.is_empty() {
            return None;
        }
        let data = std::mem::take(&mut self.current_au);
        let keyframe = self.current_is_keyframe;
        self.current_has_vcl = false;
        self.current_is_keyframe = false;
        Some(AccessUnit { data, keyframe })
    }

    fn process_nal(
        &mut self,
        codec: VideoCodec,
        nal_with_sc: &[u8],
        nal_body: &[u8],
        emitted: &mut Vec<AccessUnit>,
    ) {
        if nal_body.is_empty() {
            return;
        }
        let info = classify_nal(codec, nal_body);

        if self.current_has_vcl && info.starts_new_au
            && let Some(au) = self.take_current_au()
        {
            emitted.push(au);
        }

        self.current_au.extend_from_slice(nal_with_sc);
        if info.is_vcl {
            self.current_has_vcl = true;
        }
        if info.is_idr {
            self.current_is_keyframe = true;
        }
    }
}

/// Streaming Annex-B splitter that groups H.264 or HEVC NAL units into complete
/// access units so parameter sets (SPS/PPS/VPS) stay attached to their IDR frame
/// while emitting each frame immediately when the next picture begins.
pub struct AnnexBSplitter {
    codec: VideoCodec,
    buf: Vec<u8>,
    builder: AuBuilder,
}

impl AnnexBSplitter {
    pub fn new(codec: VideoCodec) -> Self {
        Self {
            codec,
            buf: Vec::new(),
            builder: AuBuilder::default(),
        }
    }

    pub fn push_bytes(&mut self, data: &[u8]) -> Vec<AccessUnit> {
        self.buf.extend_from_slice(data);
        let mut emitted = Vec::new();

        let Some(first_start) = find_start_code(&self.buf, 0) else {
            // Keep up to 3 trailing bytes in case a start code straddles reads.
            if self.buf.len() > 3 {
                let keep_from = self.buf.len() - 3;
                self.buf.drain(..keep_from);
            }
            return emitted;
        };

        let mut cursor = first_start.0;
        while let Some((sc_pos, sc_len)) = find_start_code(&self.buf, cursor) {
            let nal_body_start = sc_pos + sc_len;
            let Some((next_pos, _)) = find_start_code(&self.buf, nal_body_start) else {
                // Keep from `sc_pos` onward until the next start code arrives (or flush()).
                if sc_pos > 0 {
                    self.buf.drain(..sc_pos);
                }
                return emitted;
            };

            let nal_with_sc = &self.buf[sc_pos..next_pos];
            let nal_body = &self.buf[nal_body_start..next_pos];
            self.builder
                .process_nal(self.codec, nal_with_sc, nal_body, &mut emitted);
            cursor = next_pos;
        }

        if cursor > 0 {
            self.buf.drain(..cursor);
        }
        emitted
    }

    pub fn flush(&mut self) -> Vec<AccessUnit> {
        let mut emitted = Vec::new();
        if let Some((sc_pos, sc_len)) = find_start_code(&self.buf, 0) {
            let nal_with_sc = &self.buf[sc_pos..];
            let nal_body = &nal_with_sc[sc_len..];
            self.builder
                .process_nal(self.codec, nal_with_sc, nal_body, &mut emitted);
        }
        self.buf.clear();

        if let Some(last) = self.builder.take_current_au() {
            emitted.push(last);
        }
        emitted
    }
}

struct NalInfo {
    is_vcl: bool,
    is_idr: bool,
    starts_new_au: bool,
}

fn classify_nal(codec: VideoCodec, nal: &[u8]) -> NalInfo {
    match codec {
        VideoCodec::H264 => {
            let nal_type = nal[0] & 0x1f;
            let is_vcl = (1..=5).contains(&nal_type);
            let is_idr = nal_type == 5;
            // For H.264 VCL slices, `first_mb_in_slice == 0` encodes in Exp-Golomb as leading bit `1`.
            let first_slice_of_pic = is_vcl && nal.get(1).is_some_and(|b| (b & 0x80) != 0);
            // AUD (9), SEI (6), SPS (7), PPS (8), SPS ext (13) or first slice of a new picture.
            let starts_new_au =
                matches!(nal_type, 6 | 7 | 8 | 9 | 13) || first_slice_of_pic;
            NalInfo {
                is_vcl,
                is_idr,
                starts_new_au,
            }
        }
        VideoCodec::Hevc => {
            let nal_type = (nal[0] >> 1) & 0x3f;
            let is_vcl = nal_type <= 31;
            let is_idr = (16..=21).contains(&nal_type);
            // For HEVC VCL slices (2-byte header), `first_slice_segment_in_pic_flag` is MSB of byte 2.
            let first_slice_of_pic = is_vcl && nal.get(2).is_some_and(|b| (b & 0x80) != 0);
            // VPS (32), SPS (33), PPS (34), AUD (35), Prefix SEI (39)
            let starts_new_au =
                matches!(nal_type, 32 | 33 | 34 | 35 | 39) || first_slice_of_pic;
            NalInfo {
                is_vcl,
                is_idr,
                starts_new_au,
            }
        }
        VideoCodec::Av1 => NalInfo {
            is_vcl: true,
            is_idr: false,
            starts_new_au: true,
        },
    }
}

pub fn is_keyframe_annexb(codec: VideoCodec, au: &[u8]) -> bool {
    let mut cursor = 0usize;
    while let Some((sc_pos, sc_len)) = find_start_code(au, cursor) {
        let body_start = sc_pos + sc_len;
        if let Some(&first) = au.get(body_start) {
            match codec {
                VideoCodec::H264 => {
                    if (first & 0x1f) == 5 {
                        return true;
                    }
                }
                VideoCodec::Hevc => {
                    let nal_type = (first >> 1) & 0x3f;
                    if (16..=21).contains(&nal_type) {
                        return true;
                    }
                }
                VideoCodec::Av1 => {}
            }
        }
        cursor = body_start + 1;
    }
    false
}

fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    if buf.len() < from + 3 {
        return None;
    }
    let mut i = from;
    while i + 2 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if buf[i + 2] == 1 {
                return Some((i, 3));
            }
            if i + 3 < buf.len() && buf[i + 2] == 0 && buf[i + 3] == 1 {
                return Some((i, 4));
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_sps_pps_idr_and_splits_p_frames() {
        let mut stream = Vec::new();
        // Frame 1: SPS (7), PPS (8), IDR (5) with first_mb=0 (0x80)
        stream.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e]);
        stream.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
        stream.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x21]);
        // Frame 2: P-slice (1) with first_mb=0 (0x80)
        stream.extend_from_slice(&[0, 0, 1, 0x41, 0x9a, 0x22]);
        // Frame 3: P-slice (1) with first_mb=0 (0x80)
        stream.extend_from_slice(&[0, 0, 1, 0x41, 0x9a, 0x33]);

        let mut splitter = AnnexBSplitter::new(VideoCodec::H264);
        let mut aus = Vec::new();
        for chunk in stream.chunks(5) {
            aus.extend(splitter.push_bytes(chunk));
        }
        aus.extend(splitter.flush());

        assert_eq!(aus.len(), 3);
        assert!(aus[0].keyframe);
        assert!(is_keyframe_annexb(VideoCodec::H264, &aus[0].data));
        assert!(!aus[1].keyframe);
        assert!(!aus[2].keyframe);
    }

    #[test]
    fn splits_hevc_vps_sps_pps_idr_and_trail() {
        let mut stream = Vec::new();
        // VPS (32 -> 0x40), SPS (33 -> 0x42), PPS (34 -> 0x44), IDR_W_RADL (19 -> 0x26) with first_slice=1 (0x80)
        stream.extend_from_slice(&[0, 0, 0, 1, 0x40, 0x01, 0x0c]);
        stream.extend_from_slice(&[0, 0, 0, 1, 0x42, 0x01, 0x01]);
        stream.extend_from_slice(&[0, 0, 0, 1, 0x44, 0x01, 0xc0]);
        stream.extend_from_slice(&[0, 0, 0, 1, 0x26, 0x01, 0x80, 0x11]);
        // TRAIL_R (1 -> 0x02) with first_slice=1 (0x80)
        stream.extend_from_slice(&[0, 0, 0, 1, 0x02, 0x01, 0x80, 0x22]);

        let mut splitter = AnnexBSplitter::new(VideoCodec::Hevc);
        let mut aus = splitter.push_bytes(&stream);
        aus.extend(splitter.flush());

        assert_eq!(aus.len(), 2);
        assert!(aus[0].keyframe);
        assert!(is_keyframe_annexb(VideoCodec::Hevc, &aus[0].data));
        assert!(!aus[1].keyframe);
    }
}
