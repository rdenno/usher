use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio_util::sync::CancellationToken;
use usher_proto::handshake::{HelloMessage, parse_hello_line};

pub struct SshConnectConfig {
    pub host: Option<String>,
    pub local: bool,
    pub remote_bin: String,
    pub ssh_opts: Vec<String>,
    pub host_args: Vec<String>,
}

pub struct HostConnection {
    pub hello: HelloMessage,
    child: Child,
    stdin: Option<ChildStdin>,
}

impl HostConnection {
    /// Spawns either an OpenSSH publickey-only remote `usher-host` or a local
    /// `usher-host` process (for `--local` testing) and waits for `USHER_HELLO`.
    pub async fn connect(cfg: &SshConnectConfig) -> Result<Self> {
        let mut cmd = if cfg.local {
            let bin = find_local_host_bin();
            let mut c = Command::new(&bin);
            c.args(&cfg.host_args);
            c
        } else {
            let host = cfg
                .host
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("missing SSH host alias (or pass --local)"))?;
            let mut c = Command::new("ssh");
            c.args([
                "-T",
                "-o",
                "PreferredAuthentications=publickey",
                "-o",
                "PasswordAuthentication=no",
                "-o",
                "KbdInteractiveAuthentication=no",
                "-o",
                "BatchMode=yes",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=3",
            ]);
            for opt in &cfg.ssh_opts {
                c.arg("-o").arg(opt);
            }
            c.arg(host).arg("--").arg(&cfg.remote_bin).args(&cfg.host_args);
            c
        };

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                if cfg.local {
                    "failed to spawn local usher-host binary"
                } else {
                    "failed to spawn ssh process"
                }
            })?;

        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("child missing stdout"))?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!("[host] {line}");
                }
            });
        }

        let hello = tokio::time::timeout(Duration::from_secs(15), async {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next_line().await? {
                if let Some(res) = parse_hello_line(&line) {
                    return res;
                }
                tracing::debug!("[host-stdout] {line}");
            }
            bail!("host exited before sending USHER_HELLO handshake")
        })
        .await
        .context("timed out waiting 15s for USHER_HELLO from host")??;

        Ok(Self {
            hello,
            child,
            stdin,
        })
    }

    /// Runs the SSH liveness watchdog: keeps stdin open until `cancel` triggers,
    /// or cancels if the SSH / host process exits unexpectedly.
    pub async fn watch_until_shutdown(mut self, cancel: CancellationToken) {
        tokio::select! {
            _ = cancel.cancelled() => {
                // Dropping stdin delivers EOF to the remote host so it exits immediately.
                drop(self.stdin.take());
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
