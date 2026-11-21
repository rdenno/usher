use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::clipboard::MAX_CLIPBOARD_BYTES;

pub fn spawn_clipboard_task(
    enabled: bool,
    out_tx: mpsc::Sender<String>,
    mut in_rx: mpsc::Receiver<String>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    if !enabled {
        return tokio::spawn(async move {
            cancel.cancelled().await;
        });
    }

    let thread_cancel = cancel.clone();
    let handle = std::thread::Builder::new()
        .name("usher-host-clip".into())
        .spawn(move || {
            let mut cb: Option<arboard::Clipboard> = None;
            let mut retry_after = std::time::Instant::now();
            let mut logged_unavailable = false;
            let mut last_hash: Option<[u8; 32]> =
                read_clipboard(&mut cb, &mut retry_after, &mut logged_unavailable)
                    .filter(|t| !t.is_empty() && t.len() <= MAX_CLIPBOARD_BYTES)
                    .map(|t| *blake3::hash(t.as_bytes()).as_bytes());
            let mut poll_ticks = 0u32;

            while !thread_cancel.is_cancelled() {
                while let Ok(remote_text) = in_rx.try_recv() {
                    if remote_text.is_empty() || remote_text.len() > MAX_CLIPBOARD_BYTES {
                        continue;
                    }
                    let hash = *blake3::hash(remote_text.as_bytes()).as_bytes();
                    if last_hash == Some(hash) {
                        continue;
                    }
                    last_hash = Some(hash);
                    write_clipboard(&mut cb, &remote_text, &mut retry_after, &mut logged_unavailable);
                }

                poll_ticks += 1;
                if poll_ticks >= 7 {
                    poll_ticks = 0;
                    if let Some(text) =
                        read_clipboard(&mut cb, &mut retry_after, &mut logged_unavailable)
                        && !text.is_empty()
                        && text.len() <= MAX_CLIPBOARD_BYTES
                    {
                        let hash = *blake3::hash(text.as_bytes()).as_bytes();
                        if last_hash != Some(hash) {
                            last_hash = Some(hash);
                            let _ = out_tx.try_send(text);
                        }
                    }
                }

                std::thread::sleep(Duration::from_millis(60));
            }
        });

    if let Err(err) = handle {
        tracing::warn!("failed to spawn host clipboard thread: {err}");
    }

    tokio::spawn(async move {
        cancel.cancelled().await;
    })
}

#[cfg_attr(target_os = "windows", allow(dead_code))]
fn has_linux_display_vars(display: Option<&str>, wayland_display: Option<&str>) -> bool {
    display.is_some_and(|s| !s.is_empty()) || wayland_display.is_some_and(|s| !s.is_empty())
}

fn desktop_allows_clipboard() -> bool {
    #[cfg(target_os = "windows")]
    {
        if crate::windows_session::current_session_id() == 0 {
            return false;
        }
        crate::windows_session::current_input_desktop_name().as_deref() != Some("Winlogon")
    }
    #[cfg(target_os = "linux")]
    {
        let disp = std::env::var("DISPLAY").ok();
        let wdisp = std::env::var("WAYLAND_DISPLAY").ok();
        has_linux_display_vars(disp.as_deref(), wdisp.as_deref())
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        true
    }
}

#[cfg_attr(target_os = "windows", allow(dead_code))]
fn ensure_clipboard_slot<'a>(
    slot: &'a mut Option<arboard::Clipboard>,
    retry_after: &mut std::time::Instant,
    logged_unavailable: &mut bool,
) -> Option<&'a mut arboard::Clipboard> {
    if slot.is_none() {
        let now = std::time::Instant::now();
        if now < *retry_after {
            return None;
        }
        match arboard::Clipboard::new() {
            Ok(cb) => {
                *slot = Some(cb);
                *logged_unavailable = false;
            }
            Err(err) => {
                *retry_after = now + Duration::from_secs(10);
                if !*logged_unavailable {
                    tracing::debug!("clipboard unavailable in current environment ({err}); backing off");
                    *logged_unavailable = true;
                }
                return None;
            }
        }
    }
    slot.as_mut()
}

fn read_clipboard(
    slot: &mut Option<arboard::Clipboard>,
    retry_after: &mut std::time::Instant,
    logged_unavailable: &mut bool,
) -> Option<String> {
    if !desktop_allows_clipboard() {
        return None;
    }
    #[cfg(target_os = "windows")]
    {
        let _ = (retry_after, logged_unavailable);
        crate::windows_session::with_input_desktop(|name| {
            if name.as_deref() == Some("Winlogon") {
                return None;
            }
            if slot.is_none() {
                *slot = arboard::Clipboard::new().ok();
            }
            slot.as_mut().and_then(|c| c.get_text().ok())
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        ensure_clipboard_slot(slot, retry_after, logged_unavailable)
            .and_then(|c| c.get_text().ok())
    }
}

fn write_clipboard(
    slot: &mut Option<arboard::Clipboard>,
    text: &str,
    retry_after: &mut std::time::Instant,
    logged_unavailable: &mut bool,
) {
    if !desktop_allows_clipboard() {
        return;
    }
    #[cfg(target_os = "windows")]
    {
        let _ = (retry_after, logged_unavailable);
        crate::windows_session::with_input_desktop(|name| {
            if name.as_deref() == Some("Winlogon") {
                return;
            }
            if slot.is_none() {
                *slot = arboard::Clipboard::new().ok();
            }
            if let Some(c) = slot.as_mut() {
                let _ = c.set_text(text.to_string());
            }
        });
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Some(c) = ensure_clipboard_slot(slot, retry_after, logged_unavailable) {
            let _ = c.set_text(text.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_linux_clipboard_check_requires_display_env() {
        assert!(!has_linux_display_vars(None, None));
        assert!(!has_linux_display_vars(Some(""), Some("")));
        assert!(has_linux_display_vars(Some(":0"), None));
        assert!(has_linux_display_vars(None, Some("wayland-0")));
    }
}
