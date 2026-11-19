use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;
use rusty_opus::OpusDecoder;
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use usher_proto::audio::{AUDIO_CHANNELS, AUDIO_FRAME_SAMPLES_5MS, AUDIO_SAMPLE_RATE, AudioPacket};
use usher_proto::crypto::{Role, SessionCrypto};
use usher_proto::fragment::{FrameReassembler, VideoSubheader};
use usher_proto::handshake::{VideoCodec, parse_hello_line};
use usher_proto::input::InputEvent;
use usher_proto::packet::{
    ControlMessage, PacketKind, PingPayload, PongPayload, open_packet, seal_packet,
};

struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[test]
fn host_streams_encrypted_fec_video_and_opus_audio_and_exits_on_stdin_eof() {
    let bin = env!("CARGO_BIN_EXE_usher-host");
    let mut child = Command::new(bin)
        .args([
            "--bind-ip",
            "127.0.0.1",
            "--capture",
            "testsrc",
            "--width",
            "1280",
            "--height",
            "720",
            "--fps",
            "30",
            "--bitrate-kbps",
            "12000",
            "--fec",
            "25",
            "--mtu",
            "1200",
            "--no-input",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn usher-host binary");

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut guard = ChildGuard(Some(child));

    let mut reader = BufReader::new(stdout);
    let mut hello = None;
    let mut line = String::new();
    for _ in 0..32 {
        line.clear();
        let n = reader.read_line(&mut line).expect("read stdout line");
        assert!(n > 0, "usher-host closed stdout before USHER_HELLO");
        if let Some(res) = parse_hello_line(&line) {
            hello = Some(res.expect("valid USHER_HELLO"));
            break;
        }
    }
    let hello = hello.expect("expected USHER_HELLO line");
    assert_eq!(hello.bind_ip, IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(hello.width, 1280);
    assert_eq!(hello.height, 720);
    assert_eq!(hello.fps, 30);
    assert_eq!(hello.codec, VideoCodec::H264);
    assert!(hello.audio);
    assert_eq!(hello.mtu, 1200);

    let key = hello.decode_session_key().expect("valid session key");
    let mut crypto = SessionCrypto::new(&key, Role::Client);
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind client UDP");
    sock.connect(SocketAddr::new(hello.bind_ip, hello.port))
        .expect("connect client UDP");
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set timeout");

    let client_hello = seal_packet(
        &mut crypto,
        PacketKind::Control,
        &ControlMessage::ClientHello {
            client_version: 1,
            capabilities: 1,
        }
        .encode(),
        hello.mtu as usize,
    )
    .unwrap();
    sock.send(&client_hello).unwrap();

    let ping = seal_packet(
        &mut crypto,
        PacketKind::Ping,
        &PingPayload { sent_us: 987_654 }.encode(),
        hello.mtu as usize,
    )
    .unwrap();
    sock.send(&ping).unwrap();

    // Send a couple encrypted input events to verify host accepts them cleanly.
    let key_ev = seal_packet(
        &mut crypto,
        PacketKind::Input,
        &InputEvent::Key {
            evdev_code: 30,
            pressed: true,
        }
        .encode(),
        hello.mtu as usize,
    )
    .unwrap();
    sock.send(&key_ev).unwrap();

    let mut reassembler = FrameReassembler::new(8);
    let mut h264 = Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
        .expect("create OpenH264 decoder");
    let mut opus = OpusDecoder::new(AUDIO_SAMPLE_RATE as i32, AUDIO_CHANNELS as usize)
        .expect("create Opus decoder");
    let mut opus_pcm = vec![0.0f32; AUDIO_FRAME_SAMPLES_5MS * AUDIO_CHANNELS as usize * 2];

    let mut got_pong = false;
    let mut dropped_for_frame = HashSet::<u32>::new();
    let mut fec_recovered_total = 0usize;
    let mut decoded_video_frames = 0usize;
    let mut decoded_audio_frames = 0usize;

    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if got_pong
            && decoded_video_frames >= 3
            && decoded_audio_frames >= 3
            && fec_recovered_total >= 1
        {
            break;
        }
        let Ok(n) = sock.recv(&mut buf) else {
            continue;
        };
        assert!(
            n <= hello.mtu as usize,
            "UDP datagram {n} exceeded MTU {}",
            hello.mtu
        );

        let (kind, _seq, pt) = open_packet(&mut crypto, &buf[..n]).expect("valid AEAD packet");
        match kind {
            PacketKind::Pong => {
                let pong = PongPayload::decode(&pt).expect("valid Pong");
                assert_eq!(pong.sent_us, 987_654);
                got_pong = true;
            }
            PacketKind::VideoData | PacketKind::VideoFec => {
                let (sub, _) = VideoSubheader::decode_split(&pt).expect("valid subheader");
                // Intentionally drop shard 0 on multi-shard FEC-protected frames to prove live FEC recovery.
                if kind == PacketKind::VideoData
                    && sub.data_shards >= 2
                    && sub.parity_shards >= 1
                    && sub.packet_idx == 0
                    && dropped_for_frame.insert(sub.frame_id)
                {
                    continue;
                }
                let outcome = reassembler
                    .push_packet(kind, &pt)
                    .expect("valid video shard");
                if let Some(frame) = outcome.completed {
                    fec_recovered_total += frame.fec_recovered;
                    for nal in openh264::nal_units(&frame.data) {
                        if let Ok(Some(yuv)) = h264.decode(nal) {
                            assert_eq!(yuv.dimensions(), (1280, 720));
                            decoded_video_frames += 1;
                        }
                    }
                }
            }
            PacketKind::Audio => {
                let pkt = AudioPacket::decode(&pt).expect("valid AudioPacket");
                let samples = opus
                    .decode(&pkt.payload, AUDIO_FRAME_SAMPLES_5MS, &mut opus_pcm)
                    .expect("valid Opus frame");
                assert_eq!(samples, AUDIO_FRAME_SAMPLES_5MS);
                decoded_audio_frames += 1;
            }
            other => panic!("unexpected packet kind {other:?}"),
        }
    }

    assert!(
        got_pong,
        "did not receive Pong reply (video={decoded_video_frames}, audio={decoded_audio_frames}, fec={fec_recovered_total})"
    );
    assert!(
        fec_recovered_total >= 1,
        "expected XOR-FEC to recover intentionally dropped shard(s) (video={decoded_video_frames}, audio={decoded_audio_frames}, dropped={})",
        dropped_for_frame.len()
    );
    assert!(
        decoded_video_frames >= 3,
        "expected >= 3 decoded 1280x720 video frames, got {decoded_video_frames}"
    );
    assert!(
        decoded_audio_frames >= 3,
        "expected >= 3 decoded 48kHz Opus frames, got {decoded_audio_frames}"
    );

    // Closing stdin simulates SSH connection drop / EOF; host must exit cleanly right away.
    drop(stdin);
    let mut child = guard.0.take().expect("child present");
    let exit_deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(st) = child.try_wait().expect("try_wait") {
            break st;
        }
        if Instant::now() >= exit_deadline {
            let _ = child.kill();
            panic!("usher-host did not exit within 3s of stdin EOF");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(status.success(), "usher-host exited non-zero: {status}");
}
