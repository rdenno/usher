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
            let mut last_hash: Option<[u8; 32]> = read_clipboard(&mut cb)
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
                    write_clipboard(&mut cb, &remote_text);
                }

                poll_ticks += 1;
                if poll_ticks >= 7 {
                    poll_ticks = 0;
                    if let Some(text) = read_clipboard(&mut cb)
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

fn desktop_allows_clipboard() -> bool {
    #[cfg(target_os = "windows")]
    {
        if crate::windows_session::current_session_id() == 0 {
            return false;
        }
        crate::windows_session::current_input_desktop_name().as_deref() != Some("Winlogon")
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}

fn read_clipboard(slot: &mut Option<arboard::Clipboard>) -> Option<String> {
    if !desktop_allows_clipboard() {
        return None;
    }
    #[cfg(target_os = "windows")]
    {
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
        if slot.is_none() {
            *slot = arboard::Clipboard::new().ok();
        }
        slot.as_mut().and_then(|c| c.get_text().ok())
    }
}

fn write_clipboard(slot: &mut Option<arboard::Clipboard>, text: &str) {
    if !desktop_allows_clipboard() {
        return;
    }
    #[cfg(target_os = "windows")]
    {
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
        if slot.is_none() {
            *slot = arboard::Clipboard::new().ok();
        }
        if let Some(c) = slot.as_mut() {
            let _ = c.set_text(text.to_string());
        }
    }
}
