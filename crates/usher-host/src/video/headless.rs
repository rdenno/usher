use anyhow::Result;
use clap::ValueEnum;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HeadlessBackend {
    None,
    Auto,
    Sway,
    Gamescope,
    Cage,
}

#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
struct HeadlessChildren {
    compositor: Option<std::process::Child>,
    app: Option<std::process::Child>,
    app_is_explicit: bool,
}

pub struct HeadlessSession {
    backend: HeadlessBackend,
    runtime_dir: Option<PathBuf>,
    wayland_display: Option<String>,
    config_file: Option<PathBuf>,
    #[cfg(target_os = "linux")]
    children: Arc<Mutex<HeadlessChildren>>,
}

#[allow(dead_code)]
pub fn sway_config_text(width: u16, height: u16, fps: u16) -> String {
    format!(
        "output HEADLESS-1 resolution {width}x{height}@{fps}Hz\n\
         xwayland enable\n\
         default_border none\n\
         focus_follows_mouse yes\n"
    )
}

impl HeadlessSession {
    pub fn start(
        backend: HeadlessBackend,
        width: u16,
        height: u16,
        fps: u16,
        session_cmd: &[String],
    ) -> Result<Self> {
        if backend == HeadlessBackend::None {
            return Ok(Self {
                backend: HeadlessBackend::None,
                runtime_dir: None,
                wayland_display: None,
                config_file: None,
                #[cfg(target_os = "linux")]
                children: Arc::new(Mutex::new(HeadlessChildren {
                    compositor: None,
                    app: None,
                    app_is_explicit: false,
                })),
            });
        }

        #[cfg(target_os = "linux")]
        {
            linux_impl::spawn_headless(backend, width, height, fps, session_cmd)
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (width, height, fps, session_cmd);
            anyhow::bail!("headless Wayland compositors require Linux")
        }
    }

    pub fn backend(&self) -> HeadlessBackend {
        self.backend
    }

    pub fn runtime_dir(&self) -> Option<&Path> {
        self.runtime_dir.as_deref()
    }

    pub fn wayland_display(&self) -> Option<&str> {
        self.wayland_display.as_deref()
    }

    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub fn socket_path(&self) -> Option<PathBuf> {
        let rt = self.runtime_dir.as_ref()?;
        let wd = self.wayland_display.as_ref()?;
        let p = Path::new(wd);
        if p.is_absolute() {
            Some(p.to_path_buf())
        } else {
            Some(rt.join(p))
        }
    }

    pub fn watch_exit(&self, cancel: CancellationToken) {
        #[cfg(target_os = "linux")]
        if self.wayland_display.is_some() {
            let children = self.children.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tick.tick() => {
                            let exited = {
                                let mut g = children.lock().expect("headless children lock poisoned");
                                let mut reason = None;
                                if let Some(ref mut comp) = g.compositor
                                    && let Ok(Some(st)) = comp.try_wait()
                                {
                                    reason = Some(format!("headless compositor exited ({st})"));
                                }
                                if reason.is_none()
                                    && g.app_is_explicit
                                    && let Some(ref mut app) = g.app
                                    && let Ok(Some(st)) = app.try_wait()
                                {
                                    reason = Some(format!("ephemeral session command exited ({st})"));
                                }
                                reason
                            };
                            if let Some(msg) = exited {
                                tracing::info!("{msg}; tearing down session");
                                cancel.cancel();
                                break;
                            }
                        }
                    }
                }
            });
        }
        #[cfg(not(target_os = "linux"))]
        let _ = cancel;
    }
}

#[cfg(target_os = "linux")]
fn terminate_child(child: &mut std::process::Child) {
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    let pid = child.id() as libc::pid_t;
    if pid > 0 {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while std::time::Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for HeadlessSession {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Ok(mut g) = self.children.lock() {
            if let Some(mut app) = g.app.take() {
                terminate_child(&mut app);
            }
            if let Some(mut comp) = g.compositor.take() {
                terminate_child(&mut comp);
            }
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
    use super::{HeadlessBackend, HeadlessChildren, HeadlessSession, sway_config_text};
    use anyhow::{Context, Result, bail};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const DEFAULT_TERMINALS: &[&str] = &[
        "foot",
        "alacritty",
        "kitty",
        "wezterm",
        "xfce4-terminal",
        "xterm",
    ];

    fn command_in_path(bin: &str) -> bool {
        let Some(paths) = std::env::var_os("PATH") else {
            return false;
        };
        for dir in std::env::split_paths(&paths) {
            let full = dir.join(bin);
            if full.is_file()
                && fs::metadata(&full)
                    .map(|m| m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            {
                return true;
            }
        }
        false
    }

    fn resolve_backend(requested: HeadlessBackend) -> Result<HeadlessBackend> {
        match requested {
            HeadlessBackend::None => Ok(HeadlessBackend::None),
            HeadlessBackend::Auto => {
                for (cand, kind) in [
                    ("sway", HeadlessBackend::Sway),
                    ("cage", HeadlessBackend::Cage),
                    ("gamescope", HeadlessBackend::Gamescope),
                ] {
                    if command_in_path(cand) {
                        return Ok(kind);
                    }
                }
                bail!(
                    "no headless Wayland compositor found in PATH (tried sway, cage, gamescope). \
                     Install e.g. `sudo apt install sway` or `sudo pacman -S sway` / `cage`."
                );
            }
            other => Ok(other),
        }
    }

    fn find_default_terminal() -> Option<&'static str> {
        DEFAULT_TERMINALS
            .iter()
            .copied()
            .find(|t| command_in_path(t))
    }

    fn apply_wayland_app_env(
        cmd: &mut Command,
        runtime_dir: &Path,
        wayland_display: Option<&str>,
        orig_runtime_dir: Option<&Path>,
    ) {
        cmd.env("XDG_RUNTIME_DIR", runtime_dir);
        if let Some(wd) = wayland_display {
            cmd.env("WAYLAND_DISPLAY", wd);
        }
        cmd.env("MOZ_ENABLE_WAYLAND", "1")
            .env("QT_QPA_PLATFORM", "wayland")
            .env("GDK_BACKEND", "wayland");
        if std::env::var_os("PULSE_SERVER").is_none()
            && let Some(orig) = orig_runtime_dir
        {
            let pulse_sock = orig.join("pulse").join("native");
            if pulse_sock.exists() {
                cmd.env("PULSE_SERVER", format!("unix:{}", pulse_sock.display()));
            }
        }
        if let Ok(dbus) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
            cmd.env("DBUS_SESSION_BUS_ADDRESS", dbus);
        }
    }

    fn wait_for_wayland_socket(child: &mut Child, runtime_dir: &Path) -> Result<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                let mut err_tail = String::new();
                if let Some(mut se) = child.stderr.take() {
                    use std::io::Read;
                    let _ = se.read_to_string(&mut err_tail);
                }
                let detail = err_tail.trim();
                if detail.is_empty() {
                    bail!("headless compositor exited early ({status})");
                }
                bail!("headless compositor exited early ({status}): {detail}");
            }
            if let Ok(entries) = fs::read_dir(runtime_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with("wayland-") && !name.ends_with(".lock") {
                        return Ok(name);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = child.kill();
        let _ = child.wait();
        bail!(
            "timed out waiting 5s for headless Wayland socket in {}",
            runtime_dir.display()
        );
    }

    pub fn spawn_headless(
        requested: HeadlessBackend,
        width: u16,
        height: u16,
        fps: u16,
        session_cmd: &[String],
    ) -> Result<HeadlessSession> {
        let backend = resolve_backend(requested)?;
        let pid = std::process::id();
        let runtime_dir = PathBuf::from(format!("/tmp/usher-wl-{pid}"));
        fs::create_dir_all(&runtime_dir).context("failed to create XDG_RUNTIME_DIR")?;
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))?;
        let orig_runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);

        let mut config_file = None;
        let mut cmd = match backend {
            HeadlessBackend::Sway => {
                let cfg = runtime_dir.join("sway.conf");
                fs::write(&cfg, sway_config_text(width, height, fps))?;
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
                ]);
                if !session_cmd.is_empty() {
                    c.args(session_cmd);
                } else if let Some(term) = find_default_terminal() {
                    c.arg(term);
                } else {
                    c.args(["sleep", "infinity"]);
                }
                c
            }
            HeadlessBackend::Cage => {
                let mut c = Command::new("cage");
                c.arg("--");
                if !session_cmd.is_empty() {
                    c.args(session_cmd);
                } else if let Some(term) = find_default_terminal() {
                    c.arg(term);
                } else {
                    c.args(["sleep", "infinity"]);
                }
                c
            }
            HeadlessBackend::None | HeadlessBackend::Auto => unreachable!(),
        };

        apply_wayland_app_env(&mut cmd, &runtime_dir, None, orig_runtime_dir.as_deref());
        cmd.env("WLR_BACKENDS", "headless")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("WLR_HEADLESS_OUTPUTS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to launch headless compositor {backend:?}"))?;

        let wayland_display = match wait_for_wayland_socket(&mut child, &runtime_dir) {
            Ok(sock) => sock,
            Err(err) => {
                if let Some(cfg) = config_file.take() {
                    let _ = fs::remove_file(cfg);
                }
                let _ = fs::remove_dir_all(&runtime_dir);
                return Err(err);
            }
        };

        let mut app_child = None;
        let mut app_is_explicit = false;
        if backend == HeadlessBackend::Sway {
            if !session_cmd.is_empty() {
                let mut app = Command::new(&session_cmd[0]);
                app.args(&session_cmd[1..]);
                apply_wayland_app_env(
                    &mut app,
                    &runtime_dir,
                    Some(&wayland_display),
                    orig_runtime_dir.as_deref(),
                );
                app.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit());
                match app.spawn() {
                    Ok(c) => {
                        app_child = Some(c);
                        app_is_explicit = true;
                    }
                    Err(err) => {
                        super::terminate_child(&mut child);
                        if let Some(cfg) = config_file.take() {
                            let _ = fs::remove_file(cfg);
                        }
                        let _ = fs::remove_dir_all(&runtime_dir);
                        return Err(err).with_context(|| {
                            format!("failed to launch session command `{}`", session_cmd[0])
                        });
                    }
                }
            } else if let Some(term) = find_default_terminal() {
                let mut app = Command::new(term);
                apply_wayland_app_env(
                    &mut app,
                    &runtime_dir,
                    Some(&wayland_display),
                    orig_runtime_dir.as_deref(),
                );
                app.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Ok(c) = app.spawn() {
                    app_child = Some(c);
                }
            }
        } else if !session_cmd.is_empty() {
            app_is_explicit = false;
        }

        tracing::info!(
            ?backend,
            %wayland_display,
            width,
            height,
            fps,
            has_session_cmd = !session_cmd.is_empty(),
            "started ephemeral headless Wayland session"
        );

        Ok(HeadlessSession {
            backend,
            runtime_dir: Some(runtime_dir),
            wayland_display: Some(wayland_display),
            config_file,
            children: Arc::new(Mutex::new(HeadlessChildren {
                compositor: Some(child),
                app: app_child,
                app_is_explicit,
            })),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_none_is_noop() {
        let sess = HeadlessSession::start(HeadlessBackend::None, 1920, 1080, 60, &[]).unwrap();
        assert_eq!(sess.backend(), HeadlessBackend::None);
        assert!(sess.runtime_dir().is_none());
        assert!(sess.wayland_display().is_none());
        assert!(sess.socket_path().is_none());
    }

    #[test]
    fn sway_config_sets_resolution_and_fps() {
        let cfg = sway_config_text(2560, 1440, 120);
        assert!(cfg.contains("output HEADLESS-1 resolution 2560x1440@120Hz"));
        assert!(cfg.contains("xwayland enable"));
    }
}
