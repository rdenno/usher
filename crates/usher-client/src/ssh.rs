use anyhow::{Context, Result, bail};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use usher_proto::handshake::{HelloMessage, parse_hello_line};

pub struct StdioTunnelIo {
    pub stdin: ChildStdin,
    pub stdout: BufReader<ChildStdout>,
}

#[derive(Debug, Clone)]
pub struct SshConnectConfig {
    pub host: Option<String>,
    pub local: bool,
    pub remote_bin: String,
    pub ssh_args: Vec<String>,
    pub ssh_opts: Vec<String>,
    pub batch_mode: bool,
    pub auto_install: Option<bool>,
    pub host_args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTarget {
    LinuxX86_64,
    LinuxAarch64,
    WindowsX86_64,
}

impl RemoteTarget {
    fn asset_name(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "usher-linux-x86_64.tar.gz",
            Self::LinuxAarch64 => "usher-linux-aarch64.tar.gz",
            Self::WindowsX86_64 => "usher-windows-x86_64.zip",
        }
    }

    fn bin_name(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => "usher-host.exe",
            Self::LinuxX86_64 | Self::LinuxAarch64 => "usher-host",
        }
    }
}

fn current_local_target() -> Option<RemoteTarget> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some(RemoteTarget::LinuxX86_64),
        ("linux", "aarch64") => Some(RemoteTarget::LinuxAarch64),
        ("windows", "x86_64") => Some(RemoteTarget::WindowsX86_64),
        _ => None,
    }
}

/// Maps remote OS/architecture strings (from `uname -s -m` or Windows PowerShell) to a release target.
pub fn map_remote_target(os_arch_output: &str) -> Result<RemoteTarget> {
    let lower = os_arch_output.trim().to_ascii_lowercase();
    let is_linux = lower.contains("linux");
    let is_windows = lower.contains("windows") || lower.contains("mingw") || lower.contains("msys");
    let is_darwin = lower.contains("darwin") || lower.contains("macos");
    let is_x64 = lower.contains("x86_64") || lower.contains("amd64");
    let is_arm64 = lower.contains("aarch64") || lower.contains("arm64");

    if is_linux && is_x64 {
        Ok(RemoteTarget::LinuxX86_64)
    } else if is_linux && is_arm64 {
        Ok(RemoteTarget::LinuxAarch64)
    } else if is_windows && is_x64 {
        Ok(RemoteTarget::WindowsX86_64)
    } else if is_darwin {
        bail!(
            "remote host reported macOS ({}); Usher hosts currently support Linux and Windows",
            os_arch_output.trim()
        )
    } else {
        bail!(
            "unsupported remote host OS/architecture `{}` for automatic usher-host installation",
            os_arch_output.trim()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointProbeResult {
    pub endpoint: String,
    pub rtt: Option<Duration>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointProbeSummary {
    pub chosen: String,
    pub chosen_rtt: Option<Duration>,
    pub results: Vec<EndpointProbeResult>,
}

fn is_hostname_ssh_opt(opt: &str) -> bool {
    let lower = opt.trim().to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("hostname") {
        rest.starts_with('=') || rest.starts_with(' ') || rest.starts_with('\t')
    } else {
        false
    }
}

/// Returns `true` if OpenSSH arguments or `ssh_opts` already specify `-o HostName=...`.
pub fn has_explicit_hostname_override(ssh_args: &[String], ssh_opts: &[String]) -> bool {
    if ssh_opts.iter().any(|o| is_hostname_ssh_opt(o)) {
        return true;
    }
    let mut i = 0usize;
    while i < ssh_args.len() {
        let arg = &ssh_args[i];
        if arg == "-o" {
            if let Some(val) = ssh_args.get(i + 1)
                && is_hostname_ssh_opt(val)
            {
                return true;
            }
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("-o")
            && !val.is_empty()
            && is_hostname_ssh_opt(val)
        {
            return true;
        }
        i += 1;
    }
    false
}

/// Parses `port` and whether a non-`none` `ProxyJump` is configured from `ssh -G` output.
pub fn parse_ssh_g_endpoint_meta(stdout: &str) -> (u16, bool) {
    let mut port = 22u16;
    let mut uses_proxy_jump = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("port ")
            && let Ok(p) = rest.trim().parse::<u16>()
            && p > 0
        {
            port = p;
        } else if let Some(rest) = trimmed.strip_prefix("proxyjump ") {
            let val = rest.trim();
            if !val.is_empty() && !val.eq_ignore_ascii_case("none") {
                uses_proxy_jump = true;
            }
        }
    }
    (port, uses_proxy_jump)
}

/// Selects the lowest-RTT endpoint, breaking near-ties within `tie_margin` by config order.
pub fn pick_best_endpoint(
    results: &[(usize, String, Duration)],
    tie_margin: Duration,
) -> Option<(String, Duration)> {
    let min_rtt = results.iter().map(|(_, _, r)| *r).min()?;
    let cutoff = min_rtt.saturating_add(tie_margin);
    results
        .iter()
        .filter(|(_, _, r)| *r <= cutoff)
        .min_by_key(|(idx, _, _)| *idx)
        .map(|(_, ep, r)| (ep.clone(), *r))
}

async fn probe_endpoint_set(candidates: &[String], port: u16) -> Option<EndpointProbeSummary> {
    const OVERALL_TIMEOUT: Duration = Duration::from_millis(2000);
    const TIE_GRACE: Duration = Duration::from_millis(50);
    const TIE_MARGIN: Duration = Duration::from_millis(5);

    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        let ep = candidates[0].clone();
        return Some(EndpointProbeSummary {
            chosen: ep.clone(),
            chosen_rtt: None,
            results: vec![EndpointProbeResult {
                endpoint: ep,
                rtt: None,
                error: None,
            }],
        });
    }

    let overall_deadline = tokio::time::Instant::now() + OVERALL_TIMEOUT;
    let mut grace_deadline: Option<tokio::time::Instant> = None;
    let mut set = JoinSet::new();
    for (idx, ep) in candidates.iter().enumerate() {
        let ep_owned = ep.clone();
        set.spawn(async move {
            let t0 = Instant::now();
            match TcpStream::connect((ep_owned.as_str(), port)).await {
                Ok(_stream) => (idx, ep_owned, Ok(t0.elapsed())),
                Err(err) => (idx, ep_owned, Err(err.to_string())),
            }
        });
    }

    let mut outcomes: Vec<Option<EndpointProbeResult>> = vec![None; candidates.len()];
    let mut succeeded: Vec<(usize, String, Duration)> = Vec::new();

    while !set.is_empty() {
        let deadline = grace_deadline.map_or(overall_deadline, |g| g.min(overall_deadline));
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                set.abort_all();
                break;
            }
            joined = set.join_next() => {
                match joined {
                    Some(Ok((idx, ep, Ok(rtt)))) => {
                        outcomes[idx] = Some(EndpointProbeResult {
                            endpoint: ep.clone(),
                            rtt: Some(rtt),
                            error: None,
                        });
                        succeeded.push((idx, ep, rtt));
                        if grace_deadline.is_none() {
                            grace_deadline =
                                Some((tokio::time::Instant::now() + TIE_GRACE).min(overall_deadline));
                        }
                    }
                    Some(Ok((idx, ep, Err(err)))) => {
                        outcomes[idx] = Some(EndpointProbeResult {
                            endpoint: ep,
                            rtt: None,
                            error: Some(err),
                        });
                    }
                    Some(Err(_)) => {}
                    None => break,
                }
            }
        }
    }

    let (chosen, chosen_rtt) = pick_best_endpoint(&succeeded, TIE_MARGIN)?;
    let results = candidates
        .iter()
        .enumerate()
        .map(|(idx, ep)| {
            outcomes[idx].take().unwrap_or_else(|| EndpointProbeResult {
                endpoint: ep.clone(),
                rtt: None,
                error: Some("timed out / slower than winner".to_string()),
            })
        })
        .collect();

    Some(EndpointProbeSummary {
        chosen,
        chosen_rtt: Some(chosen_rtt),
        results,
    })
}

async fn query_ssh_g_endpoint_meta(
    host_alias: &str,
    ssh_args: &[String],
    ssh_opts: &[String],
) -> (u16, bool) {
    let mut cmd = Command::new("ssh");
    cmd.args(ssh_args);
    for opt in ssh_opts {
        cmd.arg("-o").arg(opt);
    }
    cmd.arg("-G").arg(host_alias);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(3), cmd.output()).await else {
        return (22, false);
    };
    if !out.status.success() {
        return (22, false);
    }
    parse_ssh_g_endpoint_meta(&String::from_utf8_lossy(&out.stdout))
}

/// Probes configured candidate endpoints concurrently on the resolved SSH TCP port and
/// selects the fastest reachable endpoint (preferring earlier order within a 5 ms tie margin).
pub async fn probe_fastest_endpoint(
    host_alias: &str,
    endpoints: &[String],
    ssh_args: &[String],
    ssh_opts: &[String],
) -> Option<EndpointProbeSummary> {
    let mut candidates = Vec::new();
    for ep in endpoints {
        let clean = ep.trim();
        if !clean.is_empty() && !candidates.iter().any(|c| c == clean) {
            candidates.push(clean.to_string());
        }
    }
    if candidates.is_empty() || has_explicit_hostname_override(ssh_args, ssh_opts) {
        return None;
    }

    let (port, uses_proxy_jump) = query_ssh_g_endpoint_meta(host_alias, ssh_args, ssh_opts).await;
    if uses_proxy_jump {
        tracing::debug!(
            host = %host_alias,
            "ProxyJump configured in SSH; skipping multi-endpoint TCP probe"
        );
        return None;
    }

    let summary = probe_endpoint_set(&candidates, port).await;
    if summary.is_none() {
        tracing::warn!(
            host = %host_alias,
            port,
            candidates = ?candidates,
            "all configured endpoints failed TCP probe; falling back to SSH config HostName"
        );
    }
    summary
}

/// Resolves SSH options with `-o HostName=<best>` prepended when multi-endpoint probing selects
/// an endpoint for a non-local target.
pub async fn resolve_ssh_opts_with_endpoints(
    host_alias: Option<&str>,
    is_local: bool,
    endpoints: &[String],
    ssh_args: &[String],
    ssh_opts: &[String],
) -> (Vec<String>, Option<EndpointProbeSummary>) {
    if is_local || endpoints.is_empty() {
        return (ssh_opts.to_vec(), None);
    }
    let Some(alias) = host_alias else {
        return (ssh_opts.to_vec(), None);
    };
    let summary = probe_fastest_endpoint(alias, endpoints, ssh_args, ssh_opts).await;
    let mut resolved = ssh_opts.to_vec();
    if let Some(ref s) = summary {
        if let Some(rtt) = s.chosen_rtt {
            tracing::info!(
                host = %alias,
                endpoint = %s.chosen,
                rtt_ms = format_args!("{:.1}", rtt.as_secs_f64() * 1000.0),
                "selected fastest SSH endpoint"
            );
        } else {
            tracing::info!(
                host = %alias,
                endpoint = %s.chosen,
                "selected configured SSH endpoint"
            );
        }
        resolved.insert(0, format!("HostName={}", s.chosen));
    }
    (resolved, summary)
}

/// Constructs OpenSSH base arguments in first-option-wins order:
/// `-T`, user passthrough flags (`ssh_args`), config `-o` opts, optional `BatchMode=yes`,
/// then Usher's keepalive/escape defaults.
pub fn build_ssh_base_args(
    ssh_args: &[String],
    ssh_opts: &[String],
    batch_mode: bool,
) -> Vec<String> {
    let mut args = Vec::with_capacity(8 + ssh_args.len() + ssh_opts.len() * 2);
    args.push("-T".to_string());
    args.extend(ssh_args.iter().cloned());
    for opt in ssh_opts {
        args.push("-o".to_string());
        args.push(opt.clone());
    }
    if batch_mode {
        args.push("-o".to_string());
        args.push("BatchMode=yes".to_string());
    }
    for (k, v) in [
        ("ServerAliveInterval", "5"),
        ("ServerAliveCountMax", "3"),
        ("EscapeChar", "none"),
    ] {
        args.push("-o".to_string());
        args.push(format!("{k}={v}"));
    }
    args
}

/// Translates `ssh` CLI arguments into `scp` CLI arguments:
/// converts `-p <port>` (and `-p<port>`) to `-P <port>`, strips SSH-only flags,
/// preserves common options (`-J`, `-i`, `-F`, `-l`, `-o`, `-c`, `-S`, `-4`, `-6`, `-C`, `-q`, `-v`),
/// and appends `-r` when transferring a directory.
pub fn build_scp_args(
    ssh_args: &[String],
    ssh_opts: &[String],
    batch_mode: bool,
    recursive: bool,
) -> Vec<String> {
    const SSH_BOOL_ONLY: &[&str] = &[
        "-T", "-t", "-A", "-a", "-N", "-M", "-n", "-X", "-x", "-Y", "-y", "-K", "-k",
    ];
    const SSH_VALUE_ONLY: &[&str] = &[
        "-L", "-R", "-D", "-W", "-b", "-B", "-E", "-e", "-I", "-m", "-Q", "-w",
    ];

    let mut args = Vec::with_capacity(8 + ssh_args.len() + ssh_opts.len() * 2);
    let mut i = 0;
    while i < ssh_args.len() {
        let arg = &ssh_args[i];
        if arg == "-p" {
            args.push("-P".to_string());
            if let Some(port) = ssh_args.get(i + 1) {
                args.push(port.clone());
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if let Some(port) = arg.strip_prefix("-p")
            && !port.is_empty()
        {
            args.push(format!("-P{port}"));
            i += 1;
            continue;
        }
        if SSH_BOOL_ONLY.contains(&arg.as_str()) {
            i += 1;
            continue;
        }
        if SSH_VALUE_ONLY.contains(&arg.as_str()) {
            i += if i + 1 < ssh_args.len() { 2 } else { 1 };
            continue;
        }
        if SSH_VALUE_ONLY
            .iter()
            .any(|prefix| arg.starts_with(prefix) && arg.len() > prefix.len())
        {
            i += 1;
            continue;
        }
        args.push(arg.clone());
        i += 1;
    }

    if recursive {
        args.push("-r".to_string());
    }
    for opt in ssh_opts {
        args.push("-o".to_string());
        args.push(opt.clone());
    }
    if batch_mode {
        args.push("-o".to_string());
        args.push("BatchMode=yes".to_string());
    }
    for (k, v) in [("ServerAliveInterval", "5"), ("ServerAliveCountMax", "3")] {
        args.push("-o".to_string());
        args.push(format!("{k}={v}"));
    }
    args
}

/// Copies a dropped local file or directory into the remote user's `Downloads/` directory via `scp`.
pub async fn transfer_file_via_scp(
    host: &str,
    ssh_args: &[String],
    ssh_opts: &[String],
    batch_mode: bool,
    local_path: &std::path::Path,
) -> Result<String> {
    let meta = tokio::fs::metadata(local_path)
        .await
        .with_context(|| format!("inspecting {}", local_path.display()))?;
    let recursive = meta.is_dir();
    let file_name = local_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "upload".to_string());
    let scp_args = build_scp_args(ssh_args, ssh_opts, batch_mode, recursive);
    let remote_dest = format!("{host}:Downloads/");

    let out = Command::new("scp")
        .args(&scp_args)
        .arg("--")
        .arg(local_path)
        .arg(&remote_dest)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("spawning scp for file transfer")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("scp failed ({}): {}", out.status, stderr.trim());
    }
    Ok(format!("Downloads/{file_name}"))
}

/// Detects whether a failed SSH/remote invocation failed because `usher-host` was missing.
pub fn is_remote_host_missing(exit_code: Option<i32>, stderr_lines: &[String]) -> bool {
    if exit_code == Some(255) {
        return false;
    }
    if exit_code == Some(127) {
        return true;
    }
    stderr_lines.iter().any(|line| {
        let lower = line.to_ascii_lowercase();
        if lower.contains("could not resolve hostname") {
            return false;
        }
        lower.contains("command not found")
            || lower.contains("is not recognized")
            || lower.contains("commandnotfoundexception")
            || lower.contains("no such file or directory")
            || (lower.contains("not found") && !lower.contains("host not found"))
    })
}

pub struct HostConnection {
    pub hello: HelloMessage,
    child: Child,
    stdio: Option<StdioTunnelIo>,
}

struct AttemptError {
    err: anyhow::Error,
    missing_remote_host: bool,
}

impl HostConnection {
    /// Spawns OpenSSH (respecting hardware keys, agent confirmation, and passphrases unless
    /// `--batch-mode` is set) or a local `usher-host` process and waits for `USHER_HELLO`.
    pub async fn connect(cfg: &SshConnectConfig) -> Result<Self> {
        match Self::connect_once(cfg, &cfg.remote_bin).await {
            Ok(conn) => Ok(conn),
            Err(attempt) => {
                if cfg.local || !attempt.missing_remote_host {
                    return Err(attempt.err);
                }
                let host = cfg
                    .host
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("missing SSH host alias"))?;
                let base_args = build_ssh_base_args(&cfg.ssh_args, &cfg.ssh_opts, cfg.batch_mode);

                if cfg.remote_bin == "usher-host"
                    && let Some(found) = probe_user_local_bin(host, &base_args).await
                {
                    tracing::info!(
                        remote_bin = %found,
                        "found usher-host in ~/.local/bin (not in non-interactive SSH PATH); launching it"
                    );
                    return Self::connect_once(cfg, &found)
                        .await
                        .map_err(|e| e.err);
                }

                let should_install = match cfg.auto_install {
                    Some(true) => true,
                    Some(false) => {
                        bail!(
                            "`usher-host` was not found on `{host}` and --no-auto-install was set.\n\
                             Install manually with:\n  \
                             Linux/macOS host setup: curl -fsSL https://raw.githubusercontent.com/rdenno/usher/main/contrib/install-host.sh | bash\n  \
                             Windows host setup:     irm https://raw.githubusercontent.com/rdenno/usher/main/contrib/install-windows.ps1 | iex"
                        );
                    }
                    None => {
                        if cfg.batch_mode || !std::io::stdin().is_terminal() {
                            bail!(
                                "`usher-host` was not found on `{host}` (non-interactive session; pass --auto-install to install automatically over SSH)"
                            );
                        }
                        prompt_confirm_install(host).await?
                    }
                };

                if !should_install {
                    return Err(attempt.err);
                }

                let installed_path = install_remote_host_over_ssh(host, &base_args).await?;
                Self::connect_once(cfg, &installed_path)
                    .await
                    .map_err(|e| e.err)
            }
        }
    }

    async fn connect_once(
        cfg: &SshConnectConfig,
        remote_bin: &str,
    ) -> std::result::Result<Self, AttemptError> {
        let mut cmd = if cfg.local {
            let bin = find_local_host_bin();
            let mut c = Command::new(&bin);
            c.args(&cfg.host_args);
            c
        } else {
            let Some(host) = cfg.host.as_deref() else {
                return Err(AttemptError {
                    err: anyhow::anyhow!("missing SSH host alias (or pass --local)"),
                    missing_remote_host: false,
                });
            };
            let mut c = Command::new("ssh");
            c.args(build_ssh_base_args(
                &cfg.ssh_args,
                &cfg.ssh_opts,
                cfg.batch_mode,
            ));
            c.arg(host).arg("--").arg(remote_bin).args(&cfg.host_args);
            c
        };

        let mut child = match cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(err) => {
                let ctx = if cfg.local {
                    "failed to spawn local usher-host binary"
                } else {
                    "failed to spawn ssh process"
                };
                return Err(AttemptError {
                    err: anyhow::Error::new(err).context(ctx),
                    missing_remote_host: false,
                });
            }
        };

        let stdin = match child.stdin.take() {
            Some(s) => s,
            None => {
                return Err(AttemptError {
                    err: anyhow::anyhow!("child missing stdin"),
                    missing_remote_host: false,
                });
            }
        };
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                return Err(AttemptError {
                    err: anyhow::anyhow!("child missing stdout"),
                    missing_remote_host: false,
                });
            }
        };

        let recent_stderr = Arc::new(Mutex::new(Vec::<String>::new()));
        let stderr_handle = child.stderr.take().map(|stderr| {
            let sink = recent_stderr.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!("[host] {line}");
                    let mut guard = sink.lock().expect("stderr log mutex poisoned");
                    if guard.len() >= 32 {
                        guard.remove(0);
                    }
                    guard.push(line);
                }
            })
        });

        let timeout_secs = if cfg.local || cfg.batch_mode { 15 } else { 60 };
        let mut reader = BufReader::new(stdout);
        let handshake_res = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).await?;
                if n == 0 {
                    bail!("host exited before sending USHER_HELLO handshake");
                }
                if let Some(res) = parse_hello_line(&line) {
                    return res;
                }
                tracing::debug!("[host-stdout] {}", line.trim_end());
            }
        })
        .await;

        match handshake_res {
            Ok(Ok(hello)) => Ok(Self {
                hello,
                child,
                stdio: Some(StdioTunnelIo {
                    stdin,
                    stdout: reader,
                }),
            }),
            Ok(Err(err)) => {
                drop(stdin);
                let status = tokio::time::timeout(Duration::from_secs(1), child.wait())
                    .await
                    .ok()
                    .and_then(|r| r.ok());
                if let Some(h) = stderr_handle {
                    let _ = tokio::time::timeout(Duration::from_millis(250), h).await;
                }
                let lines = recent_stderr
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                let missing = is_remote_host_missing(status.and_then(|s| s.code()), &lines);
                Err(AttemptError {
                    err,
                    missing_remote_host: missing,
                })
            }
            Err(_) => Err(AttemptError {
                err: anyhow::anyhow!(
                    "timed out waiting {timeout_secs}s for USHER_HELLO from host"
                ),
                missing_remote_host: false,
            }),
        }
    }

    pub fn take_stdio_tunnel(&mut self) -> Option<StdioTunnelIo> {
        self.stdio.take()
    }

    /// Runs the SSH liveness watchdog: keeps stdin open until `cancel` triggers,
    /// or cancels if the SSH / host process exits unexpectedly.
    pub async fn watch_until_shutdown(mut self, cancel: CancellationToken) {
        tokio::select! {
            _ = cancel.cancelled() => {
                // Dropping stdin delivers EOF to the remote host so it exits immediately.
                drop(self.stdio.take());
                let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
                let _ = self.child.kill().await;
            }
            status = self.child.wait() => {
                match status {
                    Ok(st) => tracing::info!("host/SSH process exited ({st}); closing client"),
                    Err(err) => tracing::warn!("host/SSH wait error: {err}"),
                }
                cancel.cancel();
            }
        }
    }
}

async fn probe_user_local_bin(host: &str, base_args: &[String]) -> Option<String> {
    let out = Command::new("ssh")
        .args(base_args)
        .arg(host)
        .arg("--")
        .arg(r#"sh -c 'test -x "$HOME/.local/bin/usher-host" && printf "%s\n" "$HOME/.local/bin/usher-host"'"#)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let candidate = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if candidate.is_empty() {
        None
    } else {
        Some(candidate)
    }
}

async fn prompt_confirm_install(host: &str) -> Result<bool> {
    use std::io::{BufRead, Write};
    let host_owned = host.to_string();
    tokio::task::spawn_blocking(move || {
        eprint!(
            "`usher-host` was not found on `{host_owned}`. Install matching `usher-host` over SSH now? [Y/n]: "
        );
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut answer)
            .context("reading confirmation response")?;
        let clean = answer.trim().to_ascii_lowercase();
        Ok(clean.is_empty() || clean == "y" || clean == "yes")
    })
    .await
    .context("auto-install confirmation task panicked")?
}

async fn probe_remote_target(host: &str, base_args: &[String]) -> Result<RemoteTarget> {
    if let Ok(out) = Command::new("ssh")
        .args(base_args)
        .arg(host)
        .arg("--")
        .arg("uname -s -m")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        && out.status.success()
    {
        let txt = String::from_utf8_lossy(&out.stdout);
        if let Ok(target) = map_remote_target(&txt) {
            return Ok(target);
        }
    }

    if let Ok(out) = Command::new("ssh")
        .args(base_args)
        .arg(host)
        .arg("--")
        .arg(r#"powershell -NoProfile -Command "Write-Output ('Windows ' + $env:PROCESSOR_ARCHITECTURE)""#)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        && out.status.success()
    {
        let txt = String::from_utf8_lossy(&out.stdout);
        return map_remote_target(&txt);
    }

    bail!(
        "could not detect remote OS/architecture on `{host}` via `uname -s -m` or PowerShell"
    );
}

async fn resolve_host_binary_bytes(target: RemoteTarget) -> Result<Vec<u8>> {
    if Some(target) == current_local_target() {
        let local = find_local_host_bin();
        if local.is_file() {
            tracing::info!(path = %local.display(), "copying matching local usher-host binary over SSH");
            return tokio::fs::read(&local)
                .await
                .with_context(|| format!("reading local {}", local.display()));
        }
    }

    let asset = target.asset_name();
    let bin_name = target.bin_name();
    let version = env!("CARGO_PKG_VERSION");
    let urls = [
        format!("https://github.com/rdenno/usher/releases/download/v{version}/{asset}"),
        format!("https://github.com/rdenno/usher/releases/latest/download/{asset}"),
    ];

    let tmp_dir = std::env::temp_dir().join(format!("usher-autoinstall-{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .with_context(|| format!("creating temp dir {}", tmp_dir.display()))?;

    let archive_path = tmp_dir.join(asset);
    let mut downloaded = false;
    for url in &urls {
        tracing::info!(%url, "downloading usher-host release asset for auto-install");
        let status = Command::new("curl")
            .args(["-fsSL", "-o"])
            .arg(&archive_path)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .await;
        if status.is_ok_and(|s| s.success()) && archive_path.is_file() {
            downloaded = true;
            break;
        }
    }
    if !downloaded {
        let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
        bail!("failed to download `{asset}` via curl from GitHub Releases");
    }

    if asset.ends_with(".tar.gz") {
        let out = Command::new("tar")
            .args(["-xzf"])
            .arg(&archive_path)
            .arg("-C")
            .arg(&tmp_dir)
            .stdin(Stdio::null())
            .output()
            .await
            .context("running tar to extract usher-host")?;
        if !out.status.success() {
            let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
            bail!(
                "tar extract failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    } else {
        #[cfg(target_os = "windows")]
        let status = Command::new("powershell")
            .args(["-NoProfile", "-Command"])
            .arg(format!(
                "Expand-Archive -Force -Path '{}' -DestinationPath '{}'",
                archive_path.display(),
                tmp_dir.display()
            ))
            .stdin(Stdio::null())
            .status()
            .await?;
        #[cfg(not(target_os = "windows"))]
        let status = Command::new("unzip")
            .arg("-q")
            .arg(&archive_path)
            .arg("-d")
            .arg(&tmp_dir)
            .stdin(Stdio::null())
            .status()
            .await?;
        if !status.success() {
            let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
            bail!("failed to extract {asset}");
        }
    }

    let extracted = tmp_dir.join(bin_name);
    let bytes = tokio::fs::read(&extracted)
        .await
        .with_context(|| format!("reading extracted {}", extracted.display()));
    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
    bytes
}

async fn install_remote_host_over_ssh(host: &str, base_args: &[String]) -> Result<String> {
    let target = probe_remote_target(host, base_args).await?;
    let bytes = resolve_host_binary_bytes(target).await?;

    let remote_cmd = match target {
        RemoteTarget::LinuxX86_64 | RemoteTarget::LinuxAarch64 => {
            r#"sh -c 'set -e; mkdir -p "$HOME/.local/bin"; cat > "$HOME/.local/bin/usher-host.tmp"; chmod 755 "$HOME/.local/bin/usher-host.tmp"; mv -f "$HOME/.local/bin/usher-host.tmp" "$HOME/.local/bin/usher-host"; printf "%s\n" "$HOME/.local/bin/usher-host"'"#
        }
        RemoteTarget::WindowsX86_64 => {
            r#"powershell -NoProfile -Command "$dir = Join-Path $env:LOCALAPPDATA 'Programs\Usher'; New-Item -ItemType Directory -Force -Path $dir | Out-Null; $dst = Join-Path $dir 'usher-host.exe'; $tmp = $dst + '.tmp'; $in = [Console]::OpenStandardInput(); $out = [System.IO.File]::Create($tmp); $in.CopyTo($out); $out.Dispose(); Move-Item -Force $tmp $dst; Write-Output $dst""#
        }
    };

    tracing::info!(target = host, bytes = bytes.len(), "uploading usher-host over SSH...");
    let mut child = Command::new("ssh")
        .args(base_args)
        .arg(host)
        .arg("--")
        .arg(remote_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning ssh to upload usher-host")?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(&bytes)
            .await
            .context("streaming usher-host binary over SSH stdin")?;
        stdin.shutdown().await.ok();
    }

    let out = child
        .wait_with_output()
        .await
        .context("waiting for remote usher-host installation")?;
    if !out.status.success() {
        bail!(
            "remote usher-host install failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let installed = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next_back()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| match target {
            RemoteTarget::WindowsX86_64 => "usher-host.exe".to_string(),
            RemoteTarget::LinuxX86_64 | RemoteTarget::LinuxAarch64 => {
                "$HOME/.local/bin/usher-host".to_string()
            }
        });

    tracing::info!(remote_bin = %installed, "installed usher-host on `{host}`");
    if matches!(target, RemoteTarget::LinuxX86_64 | RemoteTarget::LinuxAarch64) {
        tracing::info!(
            "Linux tip: run `sudo setcap cap_sys_admin+ep {installed}` on `{host}` once for zero-prompt KMS hardware capture"
        );
    }

    Ok(installed)
}

/// Queries available host displays via `usher-host --list-displays` (local or over SSH).
pub async fn query_remote_displays(
    cfg: &SshConnectConfig,
) -> Result<Vec<usher_proto::handshake::DisplayInfo>> {
    let run_once = |remote_bin: String| {
        let cfg_local = cfg.local;
        let cfg_host = cfg.host.clone();
        let ssh_args = cfg.ssh_args.clone();
        let ssh_opts = cfg.ssh_opts.clone();
        let batch_mode = cfg.batch_mode;
        let no_system = cfg.host_args.iter().any(|a| a == "--no-system");
        async move {
            let mut cmd = if cfg_local {
                let bin = find_local_host_bin();
                let mut c = Command::new(&bin);
                c.arg("--list-displays");
                if no_system {
                    c.arg("--no-system");
                }
                c
            } else {
                let host = cfg_host
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("missing SSH host alias (or pass --local)"))?;
                let mut c = Command::new("ssh");
                c.args(build_ssh_base_args(&ssh_args, &ssh_opts, batch_mode));
                c.arg(host).arg("--").arg(&remote_bin).arg("--list-displays");
                if no_system {
                    c.arg("--no-system");
                }
                c
            };
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .context("running usher-host --list-displays")
        }
    };

    let mut out = run_once(cfg.remote_bin.clone()).await?;
    if !out.status.success() && !cfg.local && cfg.remote_bin == "usher-host" {
        let stderr_lines: Vec<String> = String::from_utf8_lossy(&out.stderr)
            .lines()
            .map(ToString::to_string)
            .collect();
        if is_remote_host_missing(out.status.code(), &stderr_lines)
            && let Some(host) = cfg.host.as_deref()
        {
            let base_args = build_ssh_base_args(&cfg.ssh_args, &cfg.ssh_opts, cfg.batch_mode);
            if let Some(found) = probe_user_local_bin(host, &base_args).await {
                out = run_once(found).await?;
            }
        }
    }

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "usher-host --list-displays failed ({}): {}",
            out.status,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if let Some(res) = usher_proto::handshake::parse_displays_line(line) {
            return res;
        }
    }
    bail!(
        "usher-host --list-displays did not emit USHER_DISPLAYS (stdout={:?})",
        stdout.trim()
    )
}

fn find_local_host_bin() -> PathBuf {
    let exe_name = if cfg!(windows) {
        "usher-host.exe"
    } else {
        "usher-host"
    };
    if let Ok(cur) = std::env::current_exe()
        && let Some(dir) = cur.parent()
    {
        let sibling = dir.join(exe_name);
        if sibling.exists() {
            return sibling;
        }
    }
    PathBuf::from(exe_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ssh_base_args_places_user_args_before_defaults_and_omits_batch_mode_by_default() {
        let user = vec!["-J".into(), "bastion".into(), "-p".into(), "2222".into()];
        let opts = vec!["ControlMaster=auto".into()];

        let interactive = build_ssh_base_args(&user, &opts, false);
        assert_eq!(interactive[0], "-T");
        assert_eq!(&interactive[1..5], &user[..]);
        assert!(!interactive.iter().any(|a| a.contains("BatchMode=yes")));
        assert!(!interactive.iter().any(|a| a.contains("PasswordAuthentication=no")));
        assert!(interactive.windows(2).any(|w| w == ["-o", "ServerAliveInterval=5"]));
        let user_pos = interactive.iter().position(|a| a == "-J").unwrap();
        let opt_pos = interactive
            .iter()
            .position(|a| a == "ControlMaster=auto")
            .unwrap();
        let def_pos = interactive
            .iter()
            .position(|a| a == "ServerAliveInterval=5")
            .unwrap();
        assert!(user_pos < opt_pos && opt_pos < def_pos);

        let batch = build_ssh_base_args(&user, &opts, true);
        assert!(batch.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
    }

    #[test]
    fn detects_missing_remote_host_unix_and_windows() {
        assert!(is_remote_host_missing(Some(127), &[]));
        assert!(is_remote_host_missing(
            Some(1),
            &["bash: line 1: usher-host: command not found".into()]
        ));
        assert!(is_remote_host_missing(
            Some(9009),
            &["'usher-host' is not recognized as an internal or external command".into()]
        ));
        assert!(is_remote_host_missing(
            Some(1),
            &["CommandNotFoundException".into()]
        ));
        assert!(!is_remote_host_missing(
            Some(255),
            &["ssh: Could not resolve hostname badhost: Name or service not known".into()]
        ));
    }

    #[test]
    fn maps_uname_and_windows_arch_to_remote_target() {
        assert_eq!(
            map_remote_target("Linux x86_64").unwrap(),
            RemoteTarget::LinuxX86_64
        );
        assert_eq!(
            map_remote_target("Linux aarch64").unwrap(),
            RemoteTarget::LinuxAarch64
        );
        assert_eq!(
            map_remote_target("Windows AMD64").unwrap(),
            RemoteTarget::WindowsX86_64
        );
        assert!(map_remote_target("Darwin arm64").is_err());
    }

    #[test]
    fn build_scp_args_translates_port_and_strips_ssh_only_flags() {
        let ssh_args = vec![
            "-p".into(),
            "2222".into(),
            "-J".into(),
            "bastion".into(),
            "-i".into(),
            "~/.ssh/id_ed25519".into(),
            "-L".into(),
            "8080:localhost:80".into(),
            "-N".into(),
            "-T".into(),
            "-A".into(),
            "-X".into(),
            "-W".into(),
            "internal:22".into(),
        ];
        let opts = vec!["ControlMaster=auto".into()];
        let scp = build_scp_args(&ssh_args, &opts, true, true);
        assert!(scp.windows(2).any(|w| w == ["-P", "2222"]));
        assert!(scp.windows(2).any(|w| w == ["-J", "bastion"]));
        assert!(scp.windows(2).any(|w| w == ["-i", "~/.ssh/id_ed25519"]));
        assert!(scp.contains(&"-r".to_string()));
        assert!(scp.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert!(!scp.contains(&"-p".to_string()));
        assert!(!scp.contains(&"-L".to_string()));
        assert!(!scp.contains(&"8080:localhost:80".to_string()));
        assert!(!scp.contains(&"-N".to_string()));
        assert!(!scp.contains(&"-T".to_string()));
        assert!(!scp.contains(&"-A".to_string()));
        assert!(!scp.contains(&"-X".to_string()));
        assert!(!scp.contains(&"-W".to_string()));
        assert!(!scp.contains(&"internal:22".to_string()));
    }

    #[test]
    fn detects_explicit_hostname_override_in_ssh_args_and_opts() {
        assert!(!has_explicit_hostname_override(
            &["-p".into(), "2222".into(), "-o".into(), "User=alice".into()],
            &["ControlMaster=auto".into()],
        ));
        assert!(has_explicit_hostname_override(
            &["-o".into(), "HostName=1.2.3.4".into()],
            &[],
        ));
        assert!(has_explicit_hostname_override(
            &["-ohostname 1.2.3.4".into()],
            &[],
        ));
        assert!(has_explicit_hostname_override(
            &[],
            &["HostName=100.66.242.48".into()],
        ));
    }

    #[test]
    fn parses_ssh_g_port_and_proxyjump_meta() {
        assert_eq!(parse_ssh_g_endpoint_meta("hostname 10.0.0.1\nport 2222\n"), (2222, false));
        assert_eq!(
            parse_ssh_g_endpoint_meta("port 22\nproxyjump none\n"),
            (22, false)
        );
        assert_eq!(
            parse_ssh_g_endpoint_meta("port 2200\nproxyjump bastion\n"),
            (2200, true)
        );
    }

    #[test]
    fn picks_fastest_endpoint_with_order_tie_breaker() {
        let results = vec![
            (0usize, "ts".to_string(), Duration::from_millis(14)),
            (1usize, "zt".to_string(), Duration::from_millis(11)),
        ];
        // Within 5ms tie margin -> prefers lower configured index (0 = "ts").
        assert_eq!(
            pick_best_endpoint(&results, Duration::from_millis(5)),
            Some(("ts".to_string(), Duration::from_millis(14)))
        );

        let clear = vec![
            (0usize, "ts".to_string(), Duration::from_millis(40)),
            (1usize, "zt".to_string(), Duration::from_millis(10)),
        ];
        assert_eq!(
            pick_best_endpoint(&clear, Duration::from_millis(5)),
            Some(("zt".to_string(), Duration::from_millis(10)))
        );
        assert_eq!(pick_best_endpoint(&[], Duration::from_millis(5)), None);
    }

    #[tokio::test]
    async fn probe_endpoint_set_picks_reachable_listener_over_closed_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open_port = listener.local_addr().unwrap().port();
        let _accept = tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let summary = probe_endpoint_set(&["127.0.0.1".to_string()], open_port)
            .await
            .unwrap();
        assert_eq!(summary.chosen, "127.0.0.1");

        // Compare two live candidates on an unused address vs loopback.
        let closed_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_port = closed_listener.local_addr().unwrap().port();
        drop(closed_listener);
        assert!(probe_endpoint_set(&["127.0.0.1".to_string(), "127.0.0.1".to_string()], closed_port).await.is_none());

        // Single closed/unreachable IP fails vs open loopback on same port: use non-routable TEST-NET-1 first, loopback second.
        let candidates = vec!["192.0.2.1".to_string(), "127.0.0.1".to_string()];
        let picked = probe_endpoint_set(&candidates, open_port)
            .await
            .expect("loopback listener should win");
        assert_eq!(picked.chosen, "127.0.0.1");
        assert!(picked.chosen_rtt.is_some());
    }
}
