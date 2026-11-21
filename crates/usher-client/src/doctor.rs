use crate::config::EffectiveConfig;
use crate::config::default_config_path;
use crate::decoder::{find_working_ffmpeg, probe_client_codecs};
use crate::ssh::{HostConnection, SshConnectConfig};
use crate::transport::activate_stdio_tunnel;
use anyhow::{Context, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use usher_proto::crypto::{Role, SessionCrypto};
use usher_proto::handshake::fit_preserving_aspect;
use usher_proto::packet::{
    ControlMessage, PacketKind, PingPayload, PongPayload, decode_tunnel_frame_len,
    encode_tunnel_frame, open_packet, seal_packet,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkPath {
    DirectP2p(String),
    Relayed { relay: String, cur_addr: Option<String> },
    Unknown,
}

pub async fn run_doctor(
    host_alias: Option<&str>,
    is_local: bool,
    ssh_args: &[String],
    eff: &EffectiveConfig,
    cfg_path: &std::path::Path,
) -> Result<()> {
    println!("============================================================");
    println!("  Usher Diagnostics (v{})", env!("CARGO_PKG_VERSION"));
    println!("============================================================");
    println!(
        "[INFO] Local OS: {} ({})",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("[INFO] Config file: {}", cfg_path.display());

    // 1) Local codecs & optional ffmpeg
    #[cfg(target_os = "windows")]
    {
        let mf_decs = crate::mf_decoder::probe_mf_decoders();
        if mf_decs.is_empty() {
            println!("[WARN] Native Windows Media Foundation decoders: none detected");
        } else {
            let list = mf_decs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            println!("[OK]   Native Media Foundation HW/OS decoders: {list} (no ffmpeg required)");
        }
    }
    #[cfg(target_os = "macos")]
    {
        println!("[OK]   Native VideoToolbox HW decoders: hevc, h264 (no ffmpeg required)");
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let codecs = probe_client_codecs(eff.decoder);
        let list = codecs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        println!("[INFO] Client codecs: {list}");
    }

    if let Some(ff) = find_working_ffmpeg() {
        println!("[INFO] Optional ffmpeg fallback found: {}", ff.display());
    } else {
        println!("[INFO] Optional ffmpeg fallback: not installed (native codecs + OpenH264 active)");
    }

    // 2) SSH client availability
    match std::process::Command::new("ssh")
        .arg("-V")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(out) => {
            let ver = String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("OpenSSH")
                .trim()
                .to_string();
            println!("[OK]   SSH client: {ver}");
        }
        Err(err) => println!("[FAIL] SSH client missing from PATH ({err})"),
    }

    #[cfg(target_os = "windows")]
    check_local_windows_status();

    if !is_local && host_alias.is_none() {
        let default_cfg = default_config_path();
        println!(
            "\nTip: run `usher --doctor <ssh-host>` to test SSH bootstrap, mesh P2P vs. relay path, and live UDP RTT."
        );
        println!("Default config path: {}", default_cfg.display());
        return Ok(());
    }

    let label = if is_local {
        "local".to_string()
    } else {
        host_alias.unwrap_or_default().to_string()
    };

    // 3) Resolve SSH target IP & Tailscale/ZeroTier link path
    let (resolved_ssh_opts, probe_summary) = crate::ssh::resolve_ssh_opts_with_endpoints(
        host_alias,
        is_local,
        &eff.endpoints,
        ssh_args,
        &eff.ssh_opts,
    )
    .await;
    if let Some(summary) = probe_summary {
        println!(
            "[INFO] Multi-endpoint probe -> selected `{}`{}",
            summary.chosen,
            summary.chosen_rtt.map_or_else(String::new, |r| {
                format!(" ({:.1} ms)", r.as_secs_f64() * 1000.0)
            })
        );
        for res in &summary.results {
            if let Some(rtt) = res.rtt {
                println!(
                    "       - {}: {:.1} ms",
                    res.endpoint,
                    rtt.as_secs_f64() * 1000.0
                );
            } else if let Some(ref err) = res.error {
                println!("       - {}: unavailable ({err})", res.endpoint);
            } else {
                println!("       - {}: selected", res.endpoint);
            }
        }
    }
    let mut target_ip: Option<IpAddr> = None;
    if let Some(alias) = host_alias {
        match resolve_ssh_hostname(alias, ssh_args, &resolved_ssh_opts) {
            Some(host_str) => {
                println!("[INFO] SSH target `{alias}` -> HostName `{host_str}`");
                if let Ok(ip) = host_str.parse::<IpAddr>() {
                    target_ip = Some(ip);
                    check_mesh_path(ip);
                }
            }
            None => println!("[WARN] Could not resolve `HostName` via `ssh -G {alias}`"),
        }
    }

    // 4) Bootstrap remote host (or --local) & run encrypted UDP Ping/Pong probe
    println!("\n==> Bootstrapping `usher-host` on `{label}` (audio/input/clipboard disabled)...");
    let client_codecs = probe_client_codecs(eff.decoder);
    let codec_list = client_codecs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut host_args = vec![
        "--width".into(),
        "1280".into(),
        "--height".into(),
        "720".into(),
        "--fps".into(),
        "60".into(),
        "--codec".into(),
        eff.codec.to_string(),
        "--client-codecs".into(),
        codec_list,
        "--prefer".into(),
        eff.prefer.to_string(),
        "--mtu".into(),
        eff.mtu.to_string(),
        "--fec".into(),
        eff.fec.to_string(),
        "--no-audio".into(),
        "--no-input".into(),
        "--no-clipboard".into(),
    ];
    if let Some(ip) = eff.bind_ip {
        host_args.push("--bind-ip".into());
        host_args.push(ip.to_string());
    } else if is_local {
        host_args.push("--bind-ip".into());
        host_args.push("127.0.0.1".into());
    }

    let ssh_cfg = SshConnectConfig {
        host: host_alias.map(str::to_string),
        local: is_local,
        remote_bin: eff.remote_bin.clone(),
        ssh_args: ssh_args.to_vec(),
        ssh_opts: resolved_ssh_opts,
        batch_mode: eff.batch_mode,
        auto_install: eff.auto_install,
        host_args,
    };

    let t_ssh = Instant::now();
    let mut conn = HostConnection::connect(&ssh_cfg)
        .await
        .with_context(|| format!("failed to bootstrap usher-host on `{label}`"))?;
    let ssh_ms = t_ssh.elapsed().as_secs_f64() * 1000.0;
    let hello = conn.hello.clone();

    println!("[OK]   SSH + USHER_HELLO handshake succeeded in {ssh_ms:.0} ms");
    if !hello.host_version.is_empty() {
        println!("[INFO] Remote usher-host version: {}", hello.host_version);
    }
    println!(
        "[INFO] Ephemeral UDP endpoint: {}:{} (MTU {})",
        hello.bind_ip, hello.port, hello.mtu
    );
    if target_ip.is_none() || target_ip != Some(hello.bind_ip) {
        check_mesh_path(hello.bind_ip);
    }

    if !hello.video_backend.is_empty() {
        println!("[INFO] Remote video backend: {}", hello.video_backend);
    }
    if !hello.capture_diagnostics.is_empty() {
        println!("[INFO] Remote capture diagnostics:");
        for diag in &hello.capture_diagnostics {
            println!("       - {diag}");
        }
    }
    if !hello.gamepad_backend.is_empty() {
        println!("[INFO] Remote gamepad support: {}", hello.gamepad_backend);
    }
    let remote_codecs = hello
        .supported_codecs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if !remote_codecs.is_empty() {
        println!("[INFO] Remote supported codecs: {remote_codecs} (selected: {})", hello.codec);
    }
    for d in &hello.displays {
        let fitted = fit_preserving_aspect(d.width, d.height, 1920, 1200);
        println!(
            "[INFO] Display [{}] `{}`: {}x{} (on 1920x1200 client -> {}x{})",
            d.index, d.name, d.width, d.height, fitted.0, fitted.1
        );
    }

    // 5) Encrypted UDP Ping/Pong probe (5 pings @ 100ms)
    let session_key = hello.decode_session_key()?;
    let remote_addr = SocketAddr::new(hello.bind_ip, hello.port);
    let local_bind = match hello.bind_ip {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };
    let socket = UdpSocket::bind(local_bind).await?;
    socket.connect(remote_addr).await?;

    let crypto = Arc::new(Mutex::new(SessionCrypto::new(&session_key, Role::Client)));
    let mtu = hello.mtu as usize;
    let client_hello = ControlMessage::ClientHello {
        client_version: 2,
        capabilities: 0x03,
    };
    let pkt = {
        let mut guard = crypto.lock().expect("crypto lock poisoned");
        seal_packet(&mut guard, PacketKind::Control, &client_hello.encode(), mtu)?
    };
    let _ = socket.send(&pkt).await;

    let start = Instant::now();
    let mut rtts_ms: Vec<f64> = Vec::with_capacity(5);
    let mut buf = vec![0u8; 4096];

    for seq in 0..5u64 {
        let sent_us = start.elapsed().as_micros() as u64;
        let ping_pkt = {
            let mut guard = crypto.lock().expect("crypto lock poisoned");
            seal_packet(
                &mut guard,
                PacketKind::Ping,
                &PingPayload { sent_us }.encode(),
                mtu,
            )?
        };
        let _ = socket.send(&ping_pkt).await;

        let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
        while let Ok(Ok(n)) = tokio::time::timeout_at(deadline, socket.recv(&mut buf)).await {
            let opened = {
                let mut guard = crypto.lock().expect("crypto lock poisoned");
                open_packet(&mut guard, &buf[..n])
            };
            if let Ok((PacketKind::Pong, _, pt)) = opened
                && let Ok(pong) = PongPayload::decode(&pt)
                && pong.sent_us == sent_us
            {
                let rtt = (start.elapsed().as_micros() as u64).saturating_sub(pong.sent_us) as f64
                    / 1000.0;
                rtts_ms.push(rtt);
                break;
            }
        }
        if seq < 4 {
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    }

    let disc = {
        let mut guard = crypto.lock().expect("crypto lock poisoned");
        seal_packet(
            &mut guard,
            PacketKind::Control,
            &ControlMessage::Disconnect.encode(),
            mtu,
        )
        .ok()
    };
    if let Some(disc) = disc {
        let _ = socket.send(&disc).await;
    }

    println!("\n==> Encrypted UDP Probe Results:");
    if rtts_ms.is_empty() {
        println!("[WARN] 0/5 encrypted UDP Pong replies received (100% UDP loss).");
        if hello.tcp_tunnel
            && let Some(io) = conn.take_stdio_tunnel()
        {
            println!("==> Testing SSH TCP tunnel fallback...");
            if let Ok(mut tunnel_rtts) = probe_ssh_tcp_tunnel(io, &crypto, mtu).await
                && !tunnel_rtts.is_empty()
            {
                let min_rtt = tunnel_rtts.iter().copied().fold(f64::INFINITY, f64::min);
                let max_rtt = tunnel_rtts.iter().copied().fold(0.0_f64, f64::max);
                let avg_rtt = tunnel_rtts.drain(..).sum::<f64>() / 3.0;
                println!(
                    "[WARN] Direct UDP blocked (0/5 replies), but SSH TCP tunnel fallback succeeded (RTT min={min_rtt:.1} ms avg={avg_rtt:.1} ms max={max_rtt:.1} ms)"
                );
                println!("       Usher will automatically fall back to SSH TCP tunnel on this host.");
                drop(conn);
                return Ok(());
            }
        }
        drop(conn);
        println!("       Check firewall inbound UDP on remote `usher-host` or overlay routing.");
        anyhow::bail!("encrypted UDP probe failed (0/5 replies)");
    }
    drop(conn);

    let min_rtt = rtts_ms.iter().copied().fold(f64::INFINITY, f64::min);
    let max_rtt = rtts_ms.iter().copied().fold(0.0_f64, f64::max);
    let avg_rtt = rtts_ms.iter().sum::<f64>() / rtts_ms.len() as f64;
    let loss_pct = ((5 - rtts_ms.len()) as f64 / 5.0) * 100.0;

    if avg_rtt > 60.0 {
        println!(
            "[WARN] {}/5 replies ({loss_pct:.0}% loss) | RTT min={min_rtt:.1} ms avg={avg_rtt:.1} ms max={max_rtt:.1} ms",
            rtts_ms.len()
        );
        println!("       High RTT (>60 ms) suggests DERP/PLANET relay rather than direct P2P UDP.");
    } else {
        println!(
            "[OK]   {}/5 replies ({loss_pct:.0}% loss) | RTT min={min_rtt:.1} ms avg={avg_rtt:.1} ms max={max_rtt:.1} ms — Direct UDP healthy!",
            rtts_ms.len()
        );
    }

    Ok(())
}

async fn probe_ssh_tcp_tunnel(
    io: crate::ssh::StdioTunnelIo,
    crypto: &Mutex<SessionCrypto>,
    mtu: usize,
) -> Result<Vec<f64>> {
    let client_hello_pkt = {
        let mut guard = crypto.lock().expect("crypto lock poisoned");
        seal_packet(
            &mut guard,
            PacketKind::Control,
            &ControlMessage::ClientHello {
                client_version: 2,
                capabilities: 0x03,
            }
            .encode(),
            mtu,
        )?
    };
    let (mut stdin, mut stdout) = activate_stdio_tunnel(io, &client_hello_pkt).await?;
    let start = Instant::now();
    let mut rtts_ms = Vec::with_capacity(3);

    for seq in 0..3u64 {
        let sent_us = start.elapsed().as_micros() as u64;
        let ping_pkt = {
            let mut guard = crypto.lock().expect("crypto lock poisoned");
            seal_packet(
                &mut guard,
                PacketKind::Ping,
                &PingPayload { sent_us }.encode(),
                mtu,
            )?
        };
        let framed = encode_tunnel_frame(&ping_pkt)?;
        stdin.write_all(&framed).await?;
        stdin.flush().await?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let mut len_hdr = [0u8; 2];
            if tokio::time::timeout_at(deadline, stdout.read_exact(&mut len_hdr))
                .await
                .ok()
                .and_then(|r| r.ok())
                .is_none()
            {
                break;
            }
            let len = decode_tunnel_frame_len(len_hdr)?;
            let mut raw = vec![0u8; len];
            if tokio::time::timeout_at(deadline, stdout.read_exact(&mut raw))
                .await
                .ok()
                .and_then(|r| r.ok())
                .is_none()
            {
                break;
            }
            let opened = {
                let mut guard = crypto.lock().expect("crypto lock poisoned");
                open_packet(&mut guard, &raw)
            };
            if let Ok((PacketKind::Pong, _, pt)) = opened
                && let Ok(pong) = PongPayload::decode(&pt)
                && pong.sent_us == sent_us
            {
                let rtt = (start.elapsed().as_micros() as u64).saturating_sub(pong.sent_us) as f64
                    / 1000.0;
                rtts_ms.push(rtt);
                break;
            }
        }
        if seq < 2 {
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
    }

    let disc = {
        let mut guard = crypto.lock().expect("crypto lock poisoned");
        seal_packet(
            &mut guard,
            PacketKind::Control,
            &ControlMessage::Disconnect.encode(),
            mtu,
        )
        .ok()
    };
    if let Some(d) = disc
        && let Ok(framed) = encode_tunnel_frame(&d)
    {
        let _ = stdin.write_all(&framed).await;
        let _ = stdin.flush().await;
    }

    Ok(rtts_ms)
}

#[cfg(target_os = "windows")]
fn check_local_windows_status() {
    let fw_ok = std::process::Command::new("netsh")
        .args(["advfirewall", "firewall", "show", "rule", "name=Usher Host (UDP)"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success());
    if fw_ok {
        println!("[OK]   Windows Firewall inbound rule `Usher Host (UDP)`: present");
    } else {
        println!("[INFO] Windows Firewall inbound rule `Usher Host (UDP)`: not configured on this machine (only required if hosting)");
    }

    let vigem_ok = std::process::Command::new("sc.exe")
        .args(["query", "ViGEmBus"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("RUNNING"));
    if vigem_ok {
        println!("[INFO] Local ViGEmBus virtual gamepad driver: RUNNING");
    }
}

fn check_mesh_path(target_ip: IpAddr) {
    if target_ip.is_loopback() {
        println!("[INFO] Loopback target ({target_ip}): direct local socket");
        return;
    }

    if let Ok(out) = std::process::Command::new("tailscale")
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        match parse_tailscale_peer_status(&text, target_ip) {
            LinkPath::DirectP2p(endpoint) => {
                println!("[OK]   Tailscale path to {target_ip}: Direct P2P ({endpoint})");
                return;
            }
            LinkPath::Relayed { relay, cur_addr } => {
                println!(
                    "[WARN] Tailscale path to {target_ip}: Relayed via DERP `{relay}` (CurAddr={:?}); UDP latency may be elevated",
                    cur_addr.as_deref().unwrap_or("none")
                );
                return;
            }
            LinkPath::Unknown => {}
        }
    }

    if let Ok(out) = std::process::Command::new("zerotier-cli")
        .args(["listpeers"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        let summary = summarize_zerotier_peers(&text);
        println!("[INFO] ZeroTier peers: {summary}");
    }
}

pub fn resolve_ssh_hostname(
    alias: &str,
    ssh_args: &[String],
    ssh_opts: &[String],
) -> Option<String> {
    let mut cmd = std::process::Command::new("ssh");
    cmd.args(ssh_args);
    for opt in ssh_opts {
        cmd.arg("-o").arg(opt);
    }
    let out = cmd
        .args(["-G", alias])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ssh_g_hostname(&String::from_utf8_lossy(&out.stdout))
}

pub fn parse_ssh_g_hostname(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("hostname ") {
            let val = rest.trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

pub fn parse_tailscale_peer_status(json_str: &str, target_ip: IpAddr) -> LinkPath {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return LinkPath::Unknown;
    };
    let Some(peers) = val.get("Peer").and_then(|p| p.as_object()) else {
        return LinkPath::Unknown;
    };
    let needle = target_ip.to_string();

    for peer in peers.values() {
        let has_ip = peer
            .get("TailscaleIPs")
            .and_then(|a| a.as_array())
            .is_some_and(|ips| ips.iter().any(|ip| ip.as_str() == Some(&needle)));
        if !has_ip {
            continue;
        }
        let cur_addr = peer
            .get("CurAddr")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let relay = peer
            .get("Relay")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        if let Some(ref addr) = cur_addr {
            return LinkPath::DirectP2p(addr.clone());
        }
        if let Some(r) = relay {
            return LinkPath::Relayed {
                relay: r,
                cur_addr,
            };
        }
    }
    LinkPath::Unknown
}

pub fn summarize_zerotier_peers(stdout: &str) -> String {
    let mut direct_leaves = 0usize;
    let mut planets = 0usize;
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 7 && parts[0] == "200" && parts[1] == "listpeers" {
            let path = parts[3];
            let role = parts[6];
            if role == "PLANET" {
                planets += 1;
            } else if role == "LEAF" && path != "-" {
                direct_leaves += 1;
            }
        }
    }
    format!("{direct_leaves} direct LEAF peer(s), {planets} PLANET root(s)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_g_hostname_output() {
        let sample = "user lolag\nhostname 100.66.242.48\nport 22\n";
        assert_eq!(
            parse_ssh_g_hostname(sample),
            Some("100.66.242.48".to_string())
        );
    }

    #[test]
    fn parses_tailscale_direct_vs_derp_relay() {
        let json = r#"{
            "Peer": {
                "node1": {
                    "TailscaleIPs": ["100.126.20.91"],
                    "CurAddr": "128.61.86.164:41641",
                    "Relay": "ord"
                },
                "node2": {
                    "TailscaleIPs": ["100.66.242.48"],
                    "CurAddr": "",
                    "Relay": "nyc"
                }
            }
        }"#;
        assert_eq!(
            parse_tailscale_peer_status(json, "100.126.20.91".parse().unwrap()),
            LinkPath::DirectP2p("128.61.86.164:41641".to_string())
        );
        assert_eq!(
            parse_tailscale_peer_status(json, "100.66.242.48".parse().unwrap()),
            LinkPath::Relayed {
                relay: "nyc".to_string(),
                cur_addr: None
            }
        );
        assert_eq!(
            parse_tailscale_peer_status(json, "10.0.0.1".parse().unwrap()),
            LinkPath::Unknown
        );
    }

    #[test]
    fn summarizes_zerotier_peers_cleanly() {
        let sample = "\
200 listpeers <ztaddr> <path> <latency> <version> <role>
200 listpeers 354bdad909 74.111.45.162/28288;100;2664 -964 1.16.2 LEAF
200 listpeers 93afae5963 - 52 1.16.2 LEAF
200 listpeers cafe04eba9 84.17.53.155/9993;22392;83326 -1 - PLANET
";
        assert_eq!(
            summarize_zerotier_peers(sample),
            "1 direct LEAF peer(s), 1 PLANET root(s)"
        );
    }
}
