use anyhow::Result;
use clap::ValueEnum;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HeadlessBackend {
    None,
    Sway,
    Gamescope,
    Cage,
}

pub struct HeadlessSession {
    runtime_dir: Option<PathBuf>,
    wayland_display: Option<String>,
    config_file: Option<PathBuf>,
    #[cfg(target_os = "linux")]
    child: Option<std::process::Child>,
}

impl HeadlessSession {
    pub fn start(backend: HeadlessBackend, width: u16, height: u16, fps: u16) -> Result<Self> {
        if backend == HeadlessBackend::None {
            return Ok(Self {
                runtime_dir: None,
                wayland_display: None,
                config_file: None,
                #[cfg(target_os = "linux")]
                child: None,
            });
        }

        #[cfg(target_os = "linux")]
        {
            linux_impl::spawn_headless(backend, width, height, fps)
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (width, height, fps);
            anyhow::bail!("headless Wayland compositors require Linux")
        }
    }

    pub fn runtime_dir(&self) -> Option<&Path> {
        self.runtime_dir.as_deref()
    }

    pub fn wayland_display(&self) -> Option<&str> {
        self.wayland_display.as_deref()
    }
}

impl Drop for HeadlessSession {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(cfg) = self.config_file.take() {
            let _ = std::fs::remove_file(cfg);
        }
        if let Some(dir) = self.runtime_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::{HeadlessBackend, HeadlessSession};
    use anyhow::{Context, Result, bail};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    pub fn spawn_headless(
        backend: HeadlessBackend,
        width: u16,
        height: u16,
        fps: u16,
    ) -> Result<HeadlessSession> {
        let pid = std::process::id();
        let runtime_dir = PathBuf::from(format!("/tmp/usher-wl-{pid}"));
        fs::create_dir_all(&runtime_dir).context("failed to create XDG_RUNTIME_DIR")?;
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))?;

        let mut config_file = None;
        let mut cmd = match backend {
            HeadlessBackend::Sway => {
                let cfg = runtime_dir.join("sway.conf");
                fs::write(
                    &cfg,
                    format!(
                        "output HEADLESS-1 resolution {width}x{height}@{fps}Hz\n\
                         xwayland enable\n"
                    ),
                )?;
                let mut c = Command::new("sway");
                c.arg("-c").arg(&cfg);
                config_file = Some(cfg);
                c
            }
            HeadlessBackend::Gamescope => {
                let mut c = Command::new("gamescope");
                c.args([
                    "-W",
                    &width.to_string(),
                    "-H",
                    &height.to_string(),
                    "-r",
                    &fps.to_string(),
                    "--backend",
                    "headless",
                    "--",
                    "sleep",
                    "infinity",
                ]);
                c
            }
            HeadlessBackend::Cage => {
                let mut c = Command::new("cage");
                c.args(["--", "sleep", "infinity"]);
                c
            }
            HeadlessBackend::None => unreachable!(),
        };

        cmd.env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("WLR_HEADLESS_OUTPUTS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to launch headless compositor {backend:?}"))?;

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut found_socket = None;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                bail!("headless compositor exited early ({status})");
            }
            if let Ok(entries) = fs::read_dir(&runtime_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with("wayland-") && !name.ends_with(".lock") {
                        found_socket = Some(name);
                        break;
                    }
                }
            }
            if found_socket.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        let Some(wayland_display) = found_socket else {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_dir_all(&runtime_dir);
            bail!("timed out waiting for headless Wayland socket in {runtime_dir:?}");
        };

        tracing::info!(
            ?backend,
            %wayland_display,
            width,
            height,
            fps,
            "started ephemeral headless Wayland session"
        );

        Ok(HeadlessSession {
            runtime_dir: Some(runtime_dir),
            wayland_display: Some(wayland_display),
            config_file,
            child: Some(child),
        })
    }
}
