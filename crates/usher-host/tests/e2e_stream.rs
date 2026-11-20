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
use usher_proto::handshake::{HelloMessage, VideoCodec, parse_hello_line};
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

fn spawn_test_host(
    extra_args: &[&str],
) -> (
    ChildGuard,
    std::process::ChildStdin,
    HelloMessage,
    UdpSocket,
    SessionCrypto,
) {
    let bin = env!("CARGO_BIN_EXE_usher-host");
    let mut child = Command::new(bin)
        .args(extra_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn usher-host binary");

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let guard = ChildGuard(Some(child));

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
    let key = hello.decode_session_key().expect("valid session key");
    let crypto = SessionCrypto::new(&key, Role::Client);
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind client UDP");
    sock.connect(SocketAddr::new(hello.bind_ip, hello.port))
        .expect("connect client UDP");
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set timeout");
    (guard, stdin, hello, sock, crypto)
}

fn send_ctrl(sock: &UdpSocket, crypto: &mut SessionCrypto, mtu: u16, msg: &ControlMessage) {
    let pkt = seal_packet(crypto, PacketKind::Control, &msg.encode(), mtu as usize).unwrap();
    sock.send(&pkt).unwrap();
}

fn wait_clean_exit(mut guard: ChildGuard, stdin: std::process::ChildStdin) {
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

#[test]
fn host_streams_encrypted_fec_video_and_opus_audio_and_exits_on_stdin_eof() {
    let (guard, stdin, hello, sock, mut crypto) = spawn_test_host(&[
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
    ]);
    assert_eq!(hello.bind_ip, IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(hello.width, 1280);
    assert_eq!(hello.height, 720);
    assert_eq!(hello.fps, 30);
    assert_eq!(hello.codec, VideoCodec::H264);
    assert!(hello.audio);
    assert_eq!(hello.mtu, 1200);

    send_ctrl(
        &sock,
        &mut crypto,
        hello.mtu,
        &ControlMessage::ClientHello {
            client_version: 2,
            capabilities: 0x03,
        },
    );

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
    let mut got_status = false;
    let mut dropped_for_frame = HashSet::<u32>::new();
    let mut fec_recovered_total = 0usize;
    let mut decoded_video_frames = 0usize;
    let mut decoded_audio_frames = 0usize;

    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if got_pong
            && got_status
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
            PacketKind::Control => {
                if let ControlMessage::StreamStatus { width, height, .. } =
                    ControlMessage::decode(&pt).expect("valid Control")
                {
                    assert_eq!((width, height), (1280, 720));
                    got_status = true;
                }
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

    assert!(got_status, "did not receive initial StreamStatus");
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

    wait_clean_exit(guard, stdin);
}

fn recv_status(
    sock: &UdpSocket,
    crypto: &mut SessionCrypto,
    pred: impl Fn(&ControlMessage) -> bool,
) -> ControlMessage {
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(n) = sock.recv(&mut buf)
            && let Ok((PacketKind::Control, _, pt)) = open_packet(crypto, &buf[..n])
            && let Ok(msg) = ControlMessage::decode(&pt)
            && pred(&msg)
        {
            return msg;
        }
    }
    panic!("timed out waiting for expected StreamStatus");
}

#[test]
fn host_adapts_bitrate_and_fec_on_feedback_and_applies_reconfigure() {
    let (guard, stdin, hello, sock, mut crypto) = spawn_test_host(&[
        "--bind-ip",
        "127.0.0.1",
        "--capture",
        "testsrc",
        "--width",
        "640",
        "--height",
        "360",
        "--fps",
        "30",
        "--max-bitrate-kbps",
        "40000",
        "--bitrate-kbps",
        "20000",
        "--fec",
        "10",
        "--no-input",
    ]);

    send_ctrl(
        &sock,
        &mut crypto,
        hello.mtu,
        &ControlMessage::ClientHello {
            client_version: 2,
            capabilities: 0x03,
        },
    );

    let init = recv_status(&sock, &mut crypto, |m| {
        matches!(
            m,
            ControlMessage::StreamStatus {
                target_bitrate_kbps: 20_000,
                fec_percent: 10,
                ..
            }
        )
    });
    assert!(matches!(init, ControlMessage::StreamStatus { .. }));

    // Simulate loss -> host cuts bitrate fast and raises FEC parity.
    send_ctrl(
        &sock,
        &mut crypto,
        hello.mtu,
        &ControlMessage::NetworkFeedback {
            loss_after_fec_bps: 300,
            fec_recovery_bps: 0,
            rtt_us: 10_000,
            late_frames: 0,
            recv_kbps: 15_000,
        },
    );
    let adapted = recv_status(&sock, &mut crypto, |m| {
        matches!(
            m,
            ControlMessage::StreamStatus {
                target_bitrate_kbps,
                fec_percent,
                ..
            } if *target_bitrate_kbps < 20_000 && *fec_percent > 10
        )
    });
    assert!(matches!(adapted, ControlMessage::StreamStatus { .. }));

    // Mid-stream Reconfigure -> clamp max bitrate ceiling to 10 Mbps and mute audio.
    let mut reconf = ControlMessage::reconfigure_unchanged();
    if let ControlMessage::Reconfigure {
        ref mut max_bitrate_kbps,
        ref mut audio,
        ..
    } = reconf
    {
        *max_bitrate_kbps = 10_000;
        *audio = 1;
    }
    send_ctrl(&sock, &mut crypto, hello.mtu, &reconf);
    let reconf_status = recv_status(&sock, &mut crypto, |m| {
        matches!(
            m,
            ControlMessage::StreamStatus {
                max_bitrate_kbps: 10_000,
                target_bitrate_kbps,
                audio_enabled: false,
                ..
            } if *target_bitrate_kbps <= 10_000
        )
    });
    assert!(matches!(reconf_status, ControlMessage::StreamStatus { .. }));

    wait_clean_exit(guard, stdin);
}

#[cfg(target_os = "windows")]
#[ignore]
#[test]
fn windows_live_capture_from_session0_streams_real_frames() {
    let bin = env!("CARGO_BIN_EXE_usher-host");
    let mut child = Command::new(bin)
        .args([
            "--bind-ip",
            "127.0.0.1",
            "--capture",
            "auto",
            "--width",
            "1280",
            "--height",
            "720",
            "--fps",
            "30",
            "--codec",
            "auto",
            "--client-codecs",
            "hevc,h264",
            "--no-input",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn usher-host binary");

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let guard = ChildGuard(Some(child));

    let err_lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let err_sink = err_lines.clone();
    let err_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            eprintln!("[live-host] {line}");
            err_sink.lock().unwrap().push(line);
        }
    });

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
    eprintln!(
        "[live-test] hello codec={} displays={:?}",
        hello.codec, hello.displays
    );

    let key = hello.decode_session_key().expect("valid session key");
    let mut crypto = SessionCrypto::new(&key, Role::Client);
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind client UDP");
    sock.connect(SocketAddr::new(hello.bind_ip, hello.port))
        .expect("connect client UDP");
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set timeout");

    send_ctrl(
        &sock,
        &mut crypto,
        hello.mtu,
        &ControlMessage::ClientHello {
            client_version: 2,
            capabilities: 0x03,
        },
    );
    let ping = seal_packet(
        &mut crypto,
        PacketKind::Ping,
        &PingPayload { sent_us: 42_424 }.encode(),
        hello.mtu as usize,
    )
    .unwrap();
    sock.send(&ping).unwrap();

    let mut reassembler = FrameReassembler::new(8);
    let mut got_pong = false;
    let mut completed_frames = 0usize;
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if got_pong && completed_frames >= 5 {
            break;
        }
        let Ok(n) = sock.recv(&mut buf) else {
            continue;
        };
        let (kind, _seq, pt) = open_packet(&mut crypto, &buf[..n]).expect("valid AEAD packet");
        match kind {
            PacketKind::Pong => {
                let pong = PongPayload::decode(&pt).expect("valid Pong");
                assert_eq!(pong.sent_us, 42_424);
                got_pong = true;
            }
            PacketKind::VideoData | PacketKind::VideoFec => {
                if let Some(frame) = reassembler
                    .push_packet(kind, &pt)
                    .expect("valid video shard")
                    .completed
                {
                    assert!(!frame.data.is_empty());
                    completed_frames += 1;
                }
            }
            _ => {}
        }
    }

    assert!(got_pong, "did not receive Pong reply");
    assert!(
        completed_frames >= 5,
        "expected >= 5 real video frames, got {completed_frames}"
    );

    wait_clean_exit(guard, stdin);
    let _ = err_thread.join();

    let logs = err_lines.lock().unwrap();
    assert!(
        !logs.iter().any(|l| l.contains("OpenH264 testsrc")),
        "expected real desktop/lock-screen capture, but fell back to OpenH264 testsrc: {logs:?}"
    );
}
