use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::clipboard::MAX_CLIPBOARD_BYTES;

pub fn spawn_clipboard_thread(
    out_tx: mpsc::Sender<String>,
    mut in_rx: mpsc::Receiver<String>,
    cancel: CancellationToken,
) {
    let res = std::thread::Builder::new()
        .name("usher-client-clip".into())
        .spawn(move || {
            let mut cb: Option<arboard::Clipboard> = arboard::Clipboard::new().ok();
            let mut last_hash: Option<[u8; 32]> = cb
                .as_mut()
                .and_then(|c| c.get_text().ok())
                .filter(|t| !t.is_empty() && t.len() <= MAX_CLIPBOARD_BYTES)
                .map(|t| *blake3::hash(t.as_bytes()).as_bytes());
            let mut poll_ticks = 0u32;

            while !cancel.is_cancelled() {
                while let Ok(remote_text) = in_rx.try_recv() {
                    if remote_text.is_empty() || remote_text.len() > MAX_CLIPBOARD_BYTES {
                        continue;
                    }
                    let hash = *blake3::hash(remote_text.as_bytes()).as_bytes();
                    if last_hash == Some(hash) {
                        continue;
                    }
                    last_hash = Some(hash);
                    if cb.is_none() {
                        cb = arboard::Clipboard::new().ok();
                    }
                    if let Some(c) = cb.as_mut()
                        && let Err(err) = c.set_text(remote_text)
                    {
                        tracing::debug!("client clipboard set_text failed: {err}");
                    }
                }

                poll_ticks += 1;
                if poll_ticks >= 7 {
                    poll_ticks = 0;
                    if cb.is_none() {
                        cb = arboard::Clipboard::new().ok();
                    }
                    if let Some(c) = cb.as_mut()
                        && let Ok(text) = c.get_text()
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

    if let Err(err) = res {
        tracing::warn!("failed to spawn client clipboard thread: {err}");
    }
}
