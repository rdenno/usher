use crate::video::CaptureSource;
use crate::windows_ipc::{WorkerHelloInfo, WorkerMsg, read_worker_msg};
use anyhow::{Context, Result, bail};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};
use usher_proto::crypto::generate_session_key;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS,
    TokenPrimary, TokenSessionId,
};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
    DESKTOP_WRITEOBJECTS, GetThreadDesktop, GetUserObjectInformationW, HDESK, OpenInputDesktop,
    SetThreadDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetCurrentProcess, GetCurrentThreadId,
    OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::core::{PCWSTR, PWSTR};

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
/// while an active console session exists and desktop capture was requested.
pub fn needs_session_bridge(
    windows_worker: bool,
    system_launcher: bool,
    capture: CaptureSource,
) -> bool {
    if windows_worker || system_launcher || capture == CaptureSource::Testsrc {
        return false;
    }
    let cur = current_session_id();
    let active = active_console_session_id();
    cur == 0 && active != 0 && active != u32::MAX
}

unsafe fn open_input_desktop_handle() -> Option<HDESK> {
    unsafe {
        OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(0x01FF))
            .or_else(|_| {
                OpenInputDesktop(
                    DESKTOP_CONTROL_FLAGS(0),
                    false,
                    DESKTOP_ACCESS_FLAGS(DESKTOP_READOBJECTS.0 | DESKTOP_WRITEOBJECTS.0 | 0x0040),
                )
            })
            .ok()
    }
}

unsafe fn read_desktop_name(desk: HDESK) -> Option<String> {
    let mut buf = [0u16; 128];
    let mut needed = 0u32;
    let res = unsafe {
        GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * std::mem::size_of::<u16>()) as u32,
            Some(&mut needed),
        )
    };
    if res.is_err() {
        return None;
    }
    let units = (needed as usize / 2).min(buf.len());
    let end = buf[..units]
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(units);
    let name = String::from_utf16_lossy(&buf[..end]).trim().to_string();
    if name.is_empty() { None } else { Some(name) }
}

pub fn current_input_desktop_name() -> Option<String> {
    unsafe {
        let desk = open_input_desktop_handle()?;
        let name = read_desktop_name(desk);
        let _ = CloseDesktop(desk);
        name
    }
}

pub fn switch_thread_to_input_desktop(prev_opened: &mut Option<HDESK>) -> Option<String> {
    unsafe {
        let desk = open_input_desktop_handle()?;
        let name = read_desktop_name(desk);
        if SetThreadDesktop(desk).is_ok() {
            if let Some(old) = prev_opened.replace(desk) {
                let _ = CloseDesktop(old);
            }
            name
        } else {
            let _ = CloseDesktop(desk);
            name
        }
    }
}

/// Temporarily attaches the current thread to the active input desktop (`Winlogon` / `Default`),
/// runs `f`, restores the original thread desktop, and closes the temporary handle.
pub fn with_input_desktop<T>(f: impl FnOnce(Option<String>) -> T) -> T {
    let orig = unsafe { GetThreadDesktop(GetCurrentThreadId()) }.ok();
    let mut temp = None;
    let name = switch_thread_to_input_desktop(&mut temp);
    let out = f(name);
    if let Some(opened) = temp.take() {
        if let Some(orig_desk) = orig {
            let _ = unsafe { SetThreadDesktop(orig_desk) };
        }
        let _ = unsafe { CloseDesktop(opened) };
    }
    out
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

/// Runs inside Session 0 as `NT AUTHORITY\SYSTEM`, duplicates its primary token with
/// `TokenSessionId = target_session`, and launches `usher-host.exe --windows-worker --is-system`
/// directly on `WinSta0\Default` of that console session.
pub fn run_system_launcher(
    target_session: u32,
    bridge_pipe: &str,
    ffmpeg_path: Option<&Path>,
    no_audio: bool,
    no_input: bool,
    no_clipboard: bool,
) -> Result<()> {
    let exe_path = std::env::current_exe().context("resolving current usher-host.exe path")?;
    let mut worker_args = vec![
        "--windows-worker".to_string(),
        "--is-system".to_string(),
        "--bridge-pipe".to_string(),
        bridge_pipe.to_string(),
    ];
    if let Some(ff) = ffmpeg_path {
        worker_args.push("--ffmpeg-path".into());
        worker_args.push(ff.to_string_lossy().into_owned());
    }
    if no_audio {
        worker_args.push("--no-audio".into());
    }
    if no_input {
        worker_args.push("--no-input".into());
    }
    if no_clipboard {
        worker_args.push("--no-clipboard".into());
    }

    let mut cmd_parts = vec![quote_win_arg(&exe_path.to_string_lossy())];
    cmd_parts.extend(worker_args.iter().map(|a| quote_win_arg(a)));
    let cmd_str = cmd_parts.join(" ");
    let mut cmd_wide: Vec<u16> = cmd_str.encode_utf16().chain(Some(0)).collect();
    let mut desk_wide: Vec<u16> = "WinSta0\\Default\0".encode_utf16().collect();

    unsafe {
        let mut self_tok = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut self_tok)
            .context("OpenProcessToken failed")?;

        let mut primary_tok = HANDLE::default();
        let dup_res = DuplicateTokenEx(
            self_tok,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary_tok,
        );
        let _ = CloseHandle(self_tok);
        dup_res.context("DuplicateTokenEx failed")?;

        let sess = target_session;
        let set_res = SetTokenInformation(
            primary_tok,
            TokenSessionId,
            (&sess as *const u32).cast::<c_void>(),
            std::mem::size_of::<u32>() as u32,
        );
        if let Err(err) = set_res {
            let _ = CloseHandle(primary_tok);
            return Err(anyhow::anyhow!(
                "SetTokenInformation(TokenSessionId={target_session}) failed: {err}"
            ));
        }

        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = PWSTR(desk_wide.as_mut_ptr());
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();

        let launch_res = CreateProcessAsUserW(
            Some(primary_tok),
            PCWSTR::null(),
            Some(PWSTR(cmd_wide.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        );
        let _ = CloseHandle(primary_tok);
        launch_res.with_context(|| {
            format!("CreateProcessAsUserW into console session {target_session} failed")
        })?;
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
    }

    Ok(())
}

fn create_bridge_pipe_server() -> Result<(String, NamedPipeServer)> {
    let rand_bytes = generate_session_key()?;
    let suffix: String = rand_bytes[..8].iter().map(|b| format!("{b:02x}")).collect();
    let pipe_name = format!(r"\\.\pipe\usher-bridge-{suffix}");
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .in_buffer_size(512 * 1024)
        .out_buffer_size(64 * 1024)
        .create(&pipe_name)
        .with_context(|| format!("failed to create bridge named pipe '{pipe_name}'"))?;
    Ok((pipe_name, server))
}

async fn try_launch_via_schtasks(
    pipe_name: &str,
    server: &NamedPipeServer,
    worker_args: &[String],
    use_system: bool,
) -> Result<()> {
    let rand_bytes = generate_session_key()?;
    let suffix: String = rand_bytes[..8].iter().map(|b| format!("{b:02x}")).collect();
    let task_name = format!("UsherSession-{suffix}");
    let xml_path = std::env::temp_dir().join(format!("usher-task-{suffix}.xml"));

    let exe_path = std::env::current_exe().context("resolving current usher-host.exe path")?;
    let arg_line = worker_args
        .iter()
        .map(|a| quote_win_arg(a))
        .collect::<Vec<_>>()
        .join(" ");

    let _guard = ScheduledTaskGuard {
        task_name: task_name.clone(),
        xml_path: xml_path.clone(),
    };

    if use_system {
        let xml = build_system_task_xml(&exe_path, &arg_line);
        std::fs::write(&xml_path, &xml)
            .with_context(|| format!("writing SYSTEM task XML to {xml_path:?}"))?;
        let create_out = create_scheduled_task(&task_name, &xml_path)?;
        if !create_out.status.success() {
            bail!(
                "schtasks /Create (SYSTEM) failed ({}): {}",
                create_out.status,
                String::from_utf8_lossy(&create_out.stderr).trim()
            );
        }
    } else {
        let xml = build_task_xml(&exe_path, &arg_line, "HighestAvailable");
        std::fs::write(&xml_path, &xml)
            .with_context(|| format!("writing scheduled task XML to {xml_path:?}"))?;
        let mut create_out = create_scheduled_task(&task_name, &xml_path)?;
        if !create_out.status.success() {
            let fallback_xml = build_task_xml(&exe_path, &arg_line, "LeastPrivilege");
            std::fs::write(&xml_path, &fallback_xml)
                .with_context(|| format!("writing fallback scheduled task XML to {xml_path:?}"))?;
            create_out = create_scheduled_task(&task_name, &xml_path)?;
        }
        if !create_out.status.success() {
            bail!(
                "schtasks /Create (InteractiveToken) failed ({}): {}",
                create_out.status,
                String::from_utf8_lossy(&create_out.stderr).trim()
            );
        }
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

    tokio::time::timeout(Duration::from_secs(8), server.connect())
        .await
        .with_context(|| format!("timed out waiting 8s for worker on {pipe_name}"))?
        .context("bridge pipe accept failed")?;

    Ok(())
}

/// Spawns an ephemeral console worker in `target_session`: tries `NT AUTHORITY\SYSTEM`
/// (capturing `Winlogon` Lock Screen & UAC prompts cleanly), falling back to `InteractiveToken`
/// if the SSH user is not an Administrator.
pub async fn spawn_console_worker(
    target_session: u32,
    ffmpeg_path: Option<&Path>,
    no_audio: bool,
    no_input: bool,
    no_clipboard: bool,
) -> Result<(NamedPipeServer, WorkerHelloInfo, u32)> {
    let (pipe1, mut server1) = create_bridge_pipe_server()?;
    let mut launcher_args = vec![
        "--windows-system-launcher".to_string(),
        "--target-session".to_string(),
        target_session.to_string(),
        "--bridge-pipe".to_string(),
        pipe1.clone(),
    ];
    if let Some(ff) = ffmpeg_path {
        launcher_args.push("--ffmpeg-path".into());
        launcher_args.push(ff.to_string_lossy().into_owned());
    }
    if no_audio {
        launcher_args.push("--no-audio".into());
    }
    if no_input {
        launcher_args.push("--no-input".into());
    }
    if no_clipboard {
        launcher_args.push("--no-clipboard".into());
    }

    let mut active_server = match try_launch_via_schtasks(&pipe1, &server1, &launcher_args, true).await {
        Ok(()) => server1,
        Err(sys_err) => {
            tracing::warn!(
                "SYSTEM lock-screen console launcher unavailable ({sys_err:#}); falling back to InteractiveToken user session"
            );
            drop(server1);
            let (pipe2, server2) = create_bridge_pipe_server()?;
            let mut inter_args = vec![
                "--windows-worker".to_string(),
                "--bridge-pipe".to_string(),
                pipe2.clone(),
            ];
            if let Some(ff) = ffmpeg_path {
                inter_args.push("--ffmpeg-path".into());
                inter_args.push(ff.to_string_lossy().into_owned());
            }
            if no_audio {
                inter_args.push("--no-audio".into());
            }
            if no_input {
                inter_args.push("--no-input".into());
            }
            if no_clipboard {
                inter_args.push("--no-clipboard".into());
            }
            try_launch_via_schtasks(&pipe2, &server2, &inter_args, false).await?;
            server2
        }
    };

    server1 = active_server;
    let hello = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match read_worker_msg(&mut server1).await? {
                WorkerMsg::HelloInfo(info) => return Ok::<WorkerHelloInfo, anyhow::Error>(info),
                WorkerMsg::LogLine(line) => {
                    tracing::info!("[console-worker] {line}");
                }
                other => {
                    tracing::debug!(?other, "ignored pre-hello worker IPC message");
                }
            }
        }
    })
    .await
    .context("timed out waiting 10s for console worker HelloInfo")??;

    active_server = server1;
    let resolved_session = hello.worker_session;
    Ok((active_server, hello, resolved_session))
}

pub fn quote_win_arg(arg: &str) -> String {
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

fn encode_task_xml_utf16le(principal_xml: &str, exe_path: &Path, arguments: &str) -> Vec<u8> {
    let cmd = xml_escape(&exe_path.to_string_lossy());
    let args = xml_escape(arguments);
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Principals>
    <Principal id="Author">
{principal_xml}
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

fn build_task_xml(exe_path: &Path, arguments: &str, run_level: &str) -> Vec<u8> {
    let level = xml_escape(run_level);
    let principal = format!(
        "      <LogonType>InteractiveToken</LogonType>\n      <RunLevel>{level}</RunLevel>"
    );
    encode_task_xml_utf16le(&principal, exe_path, arguments)
}

fn build_system_task_xml(exe_path: &Path, arguments: &str) -> Vec<u8> {
    let principal = "      <UserId>S-1-5-18</UserId>\n      <RunLevel>HighestAvailable</RunLevel>";
    encode_task_xml_utf16le(principal, exe_path, arguments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_utf16le_bom(bytes: &[u8]) -> String {
        assert!(bytes.starts_with(&[0xFF, 0xFE]));
        let (chunks, remainder) = bytes[2..].as_chunks::<2>();
        assert!(remainder.is_empty());
        let code_units: Vec<u16> = chunks.iter().map(|&c| u16::from_le_bytes(c)).collect();
        String::from_utf16(&code_units).expect("valid UTF-16LE XML")
    }

    #[test]
    fn task_xml_is_utf16le_with_bom_and_interactive_principal() {
        let bytes = build_task_xml(
            Path::new(r"C:\Program Files\Usher\usher-host.exe"),
            "--windows-worker --flag \"a b\"",
            "HighestAvailable",
        );
        let decoded = decode_utf16le_bom(&bytes);
        assert!(decoded.contains("encoding=\"UTF-16\""));
        assert!(decoded.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(decoded.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(!decoded.contains("<UserId>"));
    }

    #[test]
    fn system_task_xml_uses_local_system_sid() {
        let bytes = build_system_task_xml(
            Path::new(r"C:\Program Files\Usher\usher-host.exe"),
            "--windows-system-launcher --target-session 2",
        );
        let decoded = decode_utf16le_bom(&bytes);
        assert!(decoded.contains("<UserId>S-1-5-18</UserId>"));
        assert!(decoded.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(!decoded.contains("<LogonType>"));
    }
}
