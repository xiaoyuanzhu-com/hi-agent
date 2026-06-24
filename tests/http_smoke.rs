//! Smoke test for the HTTP route surface.
//!
//! Builds the axum router via [`hi_agent::foundation::server::build`] directly. The
//! reactor seams are returned alongside so the test holds them past the
//! handlers' send into `inbound` — otherwise the receiver drops and
//! POST /api/in/text returns 503.

use std::time::Duration;

use hi_agent::mind::memory::Memory;
use hi_agent::foundation::server::{self, ServerSeams};
use hi_agent::types::{Channel, JournalEntry};
use tempfile::tempdir;
use tokio::net::TcpListener;

async fn spawn_server() -> (String, tempfile::TempDir, ServerSeams) {
    let dir = tempdir().expect("tempdir");
    let memory = Memory::open(dir.path()).await.expect("memory");
    let observatory = hi_agent::foundation::observatory::Observatory::new(None, hi_agent::body::reactor::swap_budget_chars());
    let (router, seams) = server::build(
        memory,
        dir.path().to_path_buf(),
        observatory,
        hi_agent::foundation::acp::AcpTap::new(),
        hi_agent::body::reactor::ToolRegistry::new(),
        hi_agent::body::reactor::InterruptRegistry::new(),
        hi_agent::body::presence::Presence::new(),
        None,
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    tokio::time::sleep(Duration::from_millis(20)).await;

    (format!("http://{addr}"), dir, seams)
}

/// Read every per-channel `*.jsonl` under `memory/raw/` into typed entries
/// (across all scenes, channels, and day-folders).
fn read_journal(dir: &std::path::Path) -> Vec<JournalEntry> {
    let mut out = Vec::new();
    for log in walk_files(&dir.join("memory").join("raw")) {
        if log.extension().and_then(|n| n.to_str()) != Some("jsonl") {
            continue;
        }
        let contents = std::fs::read_to_string(&log).expect("read log");
        for line in contents.lines().filter(|l| !l.trim().is_empty()) {
            out.push(serde_json::from_str(line).expect("decode journal entry"));
        }
    }
    out
}

/// Every file (recursively) under `root`; empty if `root` does not exist.
fn walk_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&path) else {
            continue;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                files.push(p);
            }
        }
    }
    files
}

#[tokio::test]
async fn post_thought_accepts_and_journals() {
    let (base, dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/in/text"))
        .header("X-HI-Scene", "alice@phone")
        .body("hi")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);

    tokio::time::sleep(Duration::from_millis(50)).await;
    let entries = read_journal(dir.path());
    assert_eq!(entries.len(), 1, "expected one journal entry, got {entries:?}");
    match &entries[0] {
        JournalEntry::SignalIn { scene, body, .. } => {
            assert_eq!(scene.0, "alice@phone");
            assert_eq!(body, "hi");
        }
        other => panic!("expected SignalIn, got {other:?}"),
    }
}

#[tokio::test]
async fn post_thought_without_scene_header_is_anonymous() {
    // X-HI-Scene is "recommended" per spec; we default missing/empty to a
    // stable anonymous scene so per-scene routing still has a key.
    let (base, dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/in/text"))
        .body("hi")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);

    tokio::time::sleep(Duration::from_millis(50)).await;
    let entries = read_journal(dir.path());
    assert_eq!(entries.len(), 1);
    match &entries[0] {
        JournalEntry::SignalIn { scene, .. } => assert_eq!(scene.0, "anonymous"),
        other => panic!("expected SignalIn, got {other:?}"),
    }
}

#[tokio::test]
async fn post_vision_journals_and_persists() {
    // A still is accepted (202), persisted as bytes, AND journaled as a vision
    // signal whose `body` is a caption — from the vision capability, or a
    // placeholder when none is configured (the case here: capabilities are never
    // initialized in this test). Perception is spawned, so we poll for the line.
    let (base, dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/in/vision"))
        .header("X-HI-Scene", "alice@phone")
        .header("Content-Type", "image/jpeg")
        .body(vec![0xFFu8, 0xD8, 0xFF, 0xD9]) // minimal JPEG-ish bytes
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);

    let mut entries = read_journal(dir.path());
    for _ in 0..40 {
        if !entries.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        entries = read_journal(dir.path());
    }
    assert_eq!(entries.len(), 1, "vision still should journal one entry, got {entries:?}");
    match &entries[0] {
        JournalEntry::SignalIn { channel, scene, body, media, .. } => {
            assert_eq!(*channel, Channel::Vision);
            assert_eq!(scene.0, "alice@phone");
            assert!(!body.is_empty(), "vision signal carries a caption (placeholder when no provider)");
            let media = media.as_ref().expect("vision signal carries media");
            assert!(media.file.ends_with(".jpg"), "relative blob path, got {}", media.file);
        }
        other => panic!("expected SignalIn, got {other:?}"),
    }

    // The bytes landed as a `.jpg` under the vision channel folder.
    let raw = dir.path().join("memory").join("raw");
    let frames = walk_files(&raw)
        .into_iter()
        .filter(|p| p.extension().and_then(|n| n.to_str()) == Some("jpg"))
        .count();
    assert_eq!(frames, 1, "expected one persisted vision frame under {raw:?}");
}

#[tokio::test]
async fn all_sensory_stubs_return_501() {
    // touch/smell/taste are still 501 in v0. /audio returns 501 only when
    // STT is unconfigured, which the test forces by never calling
    // capabilities::init_from_env — the STT global stays uninitialized, so
    // stt::available() is false.
    let (base, _dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    for ch in ["touch", "smell", "taste"] {
        let resp = client
            .post(format!("{base}/api/in/{ch}"))
            .header("X-HI-Scene", "alice@phone")
            .body("...")
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NOT_IMPLEMENTED,
            "POST /api/in/{ch} should be 501"
        );
    }

    // POST /api/in/audio with no STT configured: 501 with the new (capability-gated)
    // body.
    let resp = client
        .post(format!("{base}/api/in/audio"))
        .header("X-HI-Scene", "alice@phone")
        .header("Content-Type", "audio/wav")
        .body(vec![0u8; 16])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_IMPLEMENTED);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("STT_PROVIDER"),
        "501 body should explain the capability gate, got: {body}"
    );
}

#[tokio::test]
async fn homepage_returns_html() {
    let (base, _dir, _seams) = spawn_server().await;
    let client = reqwest::Client::new();

    let resp = client.get(format!("{base}/")).send().await.expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    assert!(
        ct.starts_with("text/html"),
        "expected text/html, got {ct:?}"
    );
    let body = resp.text().await.expect("body");
    assert!(body.contains("<html") || body.contains("<!doctype"));
}

#[tokio::test]
async fn vision_get_streams_camera_video() {
    // "Vision is video": the camera streams WebM over WS /api/in/vision/stream and
    // GET /api/in/vision plays it back — one camera session per long-poll response,
    // carrying the stream's Content-Type. The first chunk is the init segment; the
    // GET body is the concatenation of every chunk the camera sent.
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let (base, _dir, _seams) = spawn_server().await;

    // The GET blocks until a camera starts, so drive it from a task and open the
    // streaming WS after it has had time to subscribe.
    let get_base = base.clone();
    let getter = tokio::spawn(async move {
        let c = reqwest::Client::new();
        let resp = c
            .get(format!("{get_base}/api/in/vision"))
            .header("X-HI-Scene", "alice@phone")
            .send()
            .await
            .expect("send GET");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap_or("").to_string())
            .unwrap_or_default();
        let body = resp.bytes().await.expect("body");
        (ct, body)
    });

    tokio::time::sleep(Duration::from_millis(80)).await;

    let ws_url = format!(
        "{}/api/in/vision/stream?scene=alice@phone&mime=video/webm",
        base.replace("http://", "ws://")
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.expect("ws connect");
    let init = vec![0x1A, 0x45, 0xDF, 0xA3]; // EBML magic — stands in for the init segment
    let frame = vec![0x42u8, 0x82, 0x88];
    ws.send(Message::Binary(init.clone())).await.expect("send init");
    ws.send(Message::Binary(frame.clone())).await.expect("send frame");
    // Give the frames time to fan out to the GET body before closing the source.
    tokio::time::sleep(Duration::from_millis(80)).await;
    ws.close(None).await.expect("close ws");

    let (ct, body) = tokio::time::timeout(Duration::from_secs(2), getter)
        .await
        .expect("vision GET within timeout")
        .expect("getter task");
    assert!(ct.starts_with("video/webm"), "content-type echoes stream mime, got {ct:?}");
    let mut expected = init.clone();
    expected.extend_from_slice(&frame);
    assert_eq!(body.as_ref(), expected.as_slice(), "GET body is the streamed chunks");
}
