use crate::video::CaptureSource;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};
use usher_proto::crypto::generate_session_key;
use usher_proto::handshake::parse_hello_line;
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};

pub fn current_session_id() -> u32 {
    let mut session_id = 0u32;
    let pid = std::process::id();
    unsafe {
        if ProcessIdToSessionId(pid, &mut session_id).is_ok() {
            session_id
        } else {
            0
        }
    }
}

pub fn active_console_session_id() -> u32 {
    unsafe { WTSGetActiveConsoleSessionId() }
}

/// Returns true when running inside Windows Service Session 0 (e.g. an OpenSSH shell)
/// while an active interactive console session exists and desktop capture was requested.
pub fn needs_session_bridge(windows_worker: bool, capture: CaptureSource) -> bool {
    if windows_worker || capture == CaptureSource::Testsrc {
        return false;
    }
    let cur = current_session_id();
    let active = active_console_session_id();
    cur == 0 && active != 0 && active != u32::MAX
}

pub async fn connect_worker_pipe(pipe_name: &str) -> Result<NamedPipeClient> {
    ClientOptions::new()
        .open(pipe_name)
        .with_context(|| format!("failed to connect to Session 0 bridge pipe '{pipe_name}'"))
}

struct ScheduledTaskGuard {
    task_name: String,
    xml_path: PathBuf,
}

impl Drop for ScheduledTaskGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("schtasks")
            .args(["/Delete", "/F", "/TN", &self.task_name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_file(&self.xml_path);
    }
}

/// Runs inside SSH Session 0: spawns an interactive Session 1 worker via Task Scheduler
/// (`InteractiveToken`), receives `USHER_HELLO` over a local Named Pipe, prints it to
/// stdout, and relays SSH stdin EOF / Ctrl+C to tear down the Session 1 worker immediately.
pub async fn run_session_bridge(worker_args: &[String]) -> Result<()> {
    let rand_bytes = generate_session_key()?;
    let suffix: String = rand_bytes[..8].iter().map(|b| format!("{b:02x}")).collect();
    let pipe_name = format!(r"\\.\pipe\usher-bridge-{suffix}");
    let task_name = format!("UsherSession-{suffix}");
    let xml_path = std::env::temp_dir().join(format!("usher-task-{suffix}.xml"));

    let server: NamedPipeServer = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_name)
        .with_context(|| format!("failed to create bridge named pipe '{pipe_name}'"))?;

    let exe_path = std::env::current_exe().context("resolving current usher-host.exe path")?;
    let mut full_args = vec![
        "--windows-worker".to_string(),
        "--bridge-pipe".to_string(),
        pipe_name.clone(),
    ];
    full_args.extend_from_slice(worker_args);

    let arg_line = full_args
        .iter()
        .map(|a| quote_win_arg(a))
        .collect::<Vec<_>>()
        .join(" ");

    let _guard = ScheduledTaskGuard {
        task_name: task_name.clone(),
        xml_path: xml_path.clone(),
    };

    let xml = build_task_xml(&exe_path, &arg_line, "HighestAvailable");
    std::fs::write(&xml_path, &xml)
        .with_context(|| format!("writing scheduled task XML to {xml_path:?}"))?;

    let mut create_out = create_scheduled_task(&task_name, &xml_path)?;
    if !create_out.status.success() {
        let first_err = String::from_utf8_lossy(&create_out.stderr)
            .trim()
            .to_string();
        tracing::debug!(
            "schtasks /Create with HighestAvailable failed ({}): {first_err}; retrying with LeastPrivilege",
            create_out.status
        );
        let fallback_xml = build_task_xml(&exe_path, &arg_line, "LeastPrivilege");
        std::fs::write(&xml_path, &fallback_xml)
            .with_context(|| format!("writing fallback scheduled task XML to {xml_path:?}"))?;
        create_out = create_scheduled_task(&task_name, &xml_path)?;
    }
    if !create_out.status.success() {
        bail!(
            "schtasks /Create failed ({}): {}",
            create_out.status,
            String::from_utf8_lossy(&create_out.stderr).trim()
        );
    }

    let run_out = std::process::Command::new("schtasks")
        .args(["/Run", "/TN", &task_name])
        .stdin(Stdio::null())
        .output()
        .context("failed to execute schtasks /Run")?;
    if !run_out.status.success() {
        bail!(
            "schtasks /Run failed ({}): {}",
            run_out.status,
            String::from_utf8_lossy(&run_out.stderr).trim()
        );
    }

    tokio::time::timeout(Duration::from_secs(10), server.connect())
        .await
        .context("timed out waiting 10s for Session 1 usher-host worker to connect")?
        .context("bridge pipe accept failed")?;

    let (pipe_rx, pipe_tx) = tokio::io::split(server);
    let mut reader = BufReader::new(pipe_rx);
    let mut line = String::new();

    let hello_line = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                bail!("Session 1 worker disconnected before sending USHER_HELLO");
            }
            if parse_hello_line(&line).is_some() {
                return Ok(line.trim_end_matches(&['\r', '\n'][..]).to_string());
            }
            tracing::info!("[session1-worker] {}", line.trim_end());
        }
    })
    .await
    .context("timed out waiting for USHER_HELLO from Session 1 worker")??;

    {
        let mut stdout = tokio::io::stdout();
        stdout.write_all(hello_line.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    tracing::info!("Session 0 <-> Session 1 SSH bridge active");

    let mut stdin = tokio::io::stdin();
    let mut stdin_buf = [0u8; 64];
    let mut worker_line = String::new();

    loop {
        tokio::select! {
            res = stdin.read(&mut stdin_buf) => {
                match res {
                    Ok(0) | Err(_) => {
                        tracing::info!("SSH stdin closed (EOF); closing Session 1 worker bridge");
                        break;
                    }
                    Ok(_) => {}
                }
            }
            res = reader.read_line(&mut worker_line) => {
                match res {
                    Ok(0) | Err(_) => {
                        let tail = worker_line.trim_end();
                        if !tail.is_empty() {
                            tracing::info!("[session1-worker] {tail}");
                        }
                        tracing::info!("Session 1 worker exited; closing SSH bridge");
                        break;
                    }
                    Ok(_) => {
                        if worker_line.ends_with('\n') || worker_line.len() >= 4096 {
                            let msg = worker_line.trim_end();
                            if !msg.is_empty() {
                                tracing::info!("[session1-worker] {msg}");
                            }
                            worker_line.clear();
                        }
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received Ctrl+C in SSH bridge; closing Session 1 worker");
                break;
            }
        }
    }

    drop(pipe_tx);
    drop(reader);
    Ok(())
}

fn quote_win_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| c.is_whitespace() || c == '"') {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                if backslashes > 0 {
                    out.push_str(&"\\".repeat(backslashes));
                    backslashes = 0;
                }
                out.push(ch);
            }
        }
    }
    if backslashes > 0 {
        out.push_str(&"\\".repeat(backslashes * 2));
    }
    out.push('"');
    out
}

fn xml_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

fn create_scheduled_task(task_name: &str, xml_path: &Path) -> Result<std::process::Output> {
    std::process::Command::new("schtasks")
        .args([
            "/Create",
            "/F",
            "/TN",
            task_name,
            "/XML",
            &xml_path.to_string_lossy(),
        ])
        .stdin(Stdio::null())
        .output()
        .context("failed to execute schtasks /Create")
}

fn build_task_xml(exe_path: &Path, arguments: &str, run_level: &str) -> Vec<u8> {
    let cmd = xml_escape(&exe_path.to_string_lossy());
    let args = xml_escape(arguments);
    let level = xml_escape(run_level);
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>{level}</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
    <Hidden>true</Hidden>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{cmd}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    );
    let mut bytes = Vec::with_capacity(2 + xml.len() * 2);
    bytes.extend_from_slice(&[0xFF, 0xFE]);
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_xml_is_utf16le_with_bom_and_interactive_principal() {
        let bytes = build_task_xml(
            Path::new(r"C:\Program Files\Usher\usher-host.exe"),
            "--windows-worker --flag \"a b\"",
            "HighestAvailable",
        );
        assert!(bytes.starts_with(&[0xFF, 0xFE]));
        let (chunks, remainder) = bytes[2..].as_chunks::<2>();
        assert!(remainder.is_empty());
        let code_units: Vec<u16> = chunks.iter().map(|&c| u16::from_le_bytes(c)).collect();
        let decoded = String::from_utf16(&code_units).expect("valid UTF-16LE XML");
        assert!(decoded.contains("encoding=\"UTF-16\""));
        assert!(decoded.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(decoded.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(!decoded.contains("<UserId>"));
    }
}
