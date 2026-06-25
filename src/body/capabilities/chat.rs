//! The native chat popup's client — a plain localhost HTTP client to the same API
//! every other client uses. The native AppKit view
//! ([`crate::foundation::vendors::macos_chat`]) is just a presentation surface; this
//! module is the client behind it: it POSTs typed lines to `/api/in/text`, follows
//! the agent's reply (`/api/out/text`) and the live input echo (`/api/in/text`
//! observe), seeds history from `/api/history`, and feeds the view through its
//! delivery functions. Going through the real endpoints means the popup counts as
//! scene presence and warms the scene exactly like a browser or phone would.
//!
//! Effectively macOS-only (the view is AppKit); elsewhere [`init`] is a no-op.

/// Wire the native chat popup to the local server at `base_url` (e.g.
/// `http://127.0.0.1:8080/`). Registers the view's send + open handlers: a typed line
/// POSTs to the API, and the first open seeds history + follows the live streams. Call
/// once, from within the tokio runtime (it captures the current `Handle`).
pub fn init(base_url: String) {
    #[cfg(target_os = "macos")]
    macos::init(base_url);
    #[cfg(not(target_os = "macos"))]
    let _ = base_url;
}

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use futures::StreamExt as _;
    use serde::Deserialize;

    use crate::foundation::vendors::macos_chat;

    /// The fixed desktop scene the tray popup talks in (matches `gesture::install`).
    const SCENE: &str = "desktop";

    pub fn init(base_url: String) {
        let handle = tokio::runtime::Handle::current();
        let client = reqwest::Client::new();

        // Send: a typed line POSTs to /api/in/text; it renders from its own echo (the
        // input stream below), so it isn't added to the view locally.
        let send_base = base_url.clone();
        let send_client = client.clone();
        let send_handle = handle.clone();
        let on_send = move |text: String| {
            let text = text.trim().to_string();
            if text.is_empty() {
                return;
            }
            let url = format!("{send_base}api/in/text");
            let client = send_client.clone();
            send_handle.spawn(async move {
                if let Err(e) = client.post(&url).header("X-HI-Scene", SCENE).body(text).send().await
                {
                    tracing::warn!(error = %e, "chat: send failed");
                }
            });
        };

        // First open: seed history, then follow the reply + input streams. Guarded so
        // re-opening the popover doesn't double-subscribe (the view persists while the
        // popover is merely hidden).
        let started = AtomicBool::new(false);
        let open_base = base_url;
        let open_client = client;
        let open_handle = handle;
        let on_open = move || {
            if started.swap(true, Ordering::SeqCst) {
                return;
            }
            open_handle.spawn(load_history(open_client.clone(), open_base.clone()));
            open_handle.spawn(follow_replies(open_client.clone(), open_base.clone()));
            open_handle.spawn(follow_inputs(open_client.clone(), open_base.clone()));
        };

        macos_chat::set_handlers(Box::new(on_send), Box::new(on_open));
    }

    /// Seed the popup with the scene's recent conversation. Renders the worded
    /// channels (typed text + spoken transcripts) as bubbles; other channels and media
    /// are skipped (v1 is text bubbles).
    async fn load_history(client: reqwest::Client, base: String) {
        #[derive(Deserialize)]
        struct Msg {
            dir: String,
            channel: String,
            body: String,
        }
        let url = format!("{base}api/history?scene={SCENE}");
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "chat: history request failed");
                return;
            }
        };
        let msgs = match resp.json::<Vec<Msg>>().await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "chat: history decode failed");
                return;
            }
        };
        for m in msgs {
            if m.body.is_empty() || !(m.channel == "text" || m.channel == "audio") {
                continue;
            }
            if m.dir == "in" {
                macos_chat::append_in(&m.body);
            } else {
                macos_chat::append_out(&m.body);
            }
        }
    }

    /// Follow the agent's reply: one utterance per long-poll, re-subscribing after
    /// each. The body streams chunks; we decode the valid UTF-8 prefix (chunks can
    /// split a multibyte char) and push the growing text into one bubble, finalizing it
    /// when the body closes (end of utterance).
    async fn follow_replies(client: reqwest::Client, base: String) {
        let url = format!("{base}api/out/text");
        loop {
            match client.get(&url).header("X-HI-Scene", SCENE).send().await {
                Ok(resp) => {
                    let mut stream = resp.bytes_stream();
                    let mut buf: Vec<u8> = Vec::new();
                    let mut emitted = false;
                    while let Some(chunk) = stream.next().await {
                        let Ok(bytes) = chunk else { break };
                        buf.extend_from_slice(&bytes);
                        let valid = match std::str::from_utf8(&buf) {
                            Ok(s) => s,
                            Err(e) => std::str::from_utf8(&buf[..e.valid_up_to()]).unwrap_or(""),
                        };
                        if !valid.is_empty() {
                            emitted = true;
                            macos_chat::agent_set(valid);
                        }
                    }
                    if emitted {
                        macos_chat::agent_end();
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "chat: reply stream failed");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }

    /// Follow the live input echo: the user's typed + spoken lines, NDJSON, one
    /// persistent stream. `final` settles a rolling transcript into a bubble.
    async fn follow_inputs(client: reqwest::Client, base: String) {
        #[derive(Deserialize)]
        struct Ev {
            text: String,
            #[serde(rename = "final")]
            is_final: bool,
        }
        let url = format!("{base}api/in/text");
        loop {
            match client.get(&url).header("X-HI-Scene", SCENE).send().await {
                Ok(resp) => {
                    let mut stream = resp.bytes_stream();
                    let mut buf: Vec<u8> = Vec::new();
                    while let Some(chunk) = stream.next().await {
                        let Ok(bytes) = chunk else { break };
                        buf.extend_from_slice(&bytes);
                        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = buf.drain(..=pos).collect();
                            let line = &line[..line.len().saturating_sub(1)];
                            if line.is_empty() {
                                continue;
                            }
                            if let Ok(ev) = serde_json::from_slice::<Ev>(line) {
                                if ev.is_final {
                                    macos_chat::in_final(&ev.text);
                                } else {
                                    macos_chat::partial_in(&ev.text);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "chat: input stream failed");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }
}
